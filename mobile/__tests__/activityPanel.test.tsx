import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import type { RunnerClient } from '../src/runnerClient';
import { ActivityPanel } from '../src/screens/ActivityPanel';
import { activityEntryMatchesPage, applyActivityEvent, seedActivityFromList } from '../src/todoActivity';
import { DARK } from '../src/ui';

const SESSION = 'activity-session';
const OTHER = 'other-session';
const LIVE = { id: 'j1', agentId: 'a1', agent: 'worker', description: 'Investigate rendering', status: 'running', createdAt: 1 };
const DONE = { ...LIVE, id: 'j2', status: 'completed', description: 'Finished task', result: { output: 'Delivered answer' }, finishedAt: 2 };
const MESSAGE = { id: 'm1', from: 'worker', to: 'parent', body: 'Mailbox greeting', timestamp: 3 };
const SECRET = 'sk-abcdefghijklmnopqrstuvwx';
function list(jobs: unknown[] = [LIVE], messages: unknown = []) { return { success: true, data: { jobs, messages } }; }
function deferred() {
  let resolve!: (value: unknown) => void;
  const promise = new Promise<unknown>((done) => { resolve = done; });
  return { promise, resolve };
}
function mockClient(handler: (sessionId?: string) => unknown = () => list()) {
  const calls: unknown[] = [];
  const listeners: Array<(frame: unknown) => void> = [];
  let listener: ((frame: unknown) => void) | null = null;
  const client = {
    command: async (command: unknown, sessionId?: string) => { calls.push({ command, sessionId }); return handler(sessionId); },
    subscribeFrames: (next: (frame: unknown) => void) => { listener = next; listeners.push(next); return () => { if (listener === next) listener = null; }; },
  } as unknown as RunnerClient;
  return { client, calls, listeners, emit: (frame: unknown) => listener?.(frame) };
}
function panel(client: RunnerClient, onError = jest.fn(), sessionId = SESSION, onClose = jest.fn()) {
  return <ActivityPanel palette={DARK} client={client} sessionId={sessionId} onError={onError} onClose={onClose} />;
}
function text(tree: renderer.ReactTestRenderer) {
  return tree.root.findAllByType(Text).flatMap((node) => typeof node.props.children === 'string' ? [node.props.children] : []).join('\n');
}
async function press(tree: renderer.ReactTestRenderer, label: string) { await act(async () => { tree.root.findByProps({ accessibilityLabel: label }).props.onPress(); }); }
async function withPanel(client: RunnerClient, run: (tree: renderer.ReactTestRenderer) => Promise<void>, onError = jest.fn(), onClose = jest.fn()) {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(panel(client, onError, SESSION, onClose)); });
  try { await run(tree); } finally { await act(async () => { tree.unmount(); }); }
}

it('ignores foreign and stale events, replaces jobs, appends messages, and renames actors', () => {
  const seed = seedActivityFromList(SESSION, [LIVE], []);
  expect(applyActivityEvent(seed, { type: 'job_updated', sessionId: OTHER, sequence: 10, job: DONE })).toBe(seed);
  expect(applyActivityEvent(seed, { type: 'job_updated', sessionId: SESSION, sequence: 0, job: DONE })).toBe(seed);
  const updated = applyActivityEvent(seed, { type: 'job_updated', sessionId: SESSION, sequence: 1, job: { ...DONE, id: LIVE.id } });
  expect(updated.entries.map((entry) => entry.summary)).toEqual(['Delivered answer']);
  const message = applyActivityEvent(updated, { type: 'message_delivered', sessionId: SESSION, sequence: 2, message: MESSAGE });
  expect(message.entries.map((entry) => entry.kind)).toEqual(['job', 'message']);
  const renamed = applyActivityEvent(message, { type: 'agent_updated', sessionId: SESSION, sequence: 3, agent: { id: 'a1', displayName: 'Researcher' } });
  expect(renamed.entries[0].actor).toBe('Researcher');
  expect(renamed.actors).toEqual({ a1: 'Researcher' });
  expect(applyActivityEvent(renamed, { type: 'message_delivered', sessionId: SESSION, sequence: 2, message: MESSAGE })).toBe(renamed);
});

it('keeps the newest 200 entries ordered by timestamp then sequence', () => {
  let snapshot = seedActivityFromList(SESSION, [], []);
  for (let sequence = 1; sequence <= 205; sequence += 1) snapshot = applyActivityEvent(snapshot, { type: 'message_delivered', sessionId: SESSION, sequence, message: { ...MESSAGE, id: String(sequence), timestamp: sequence } });
  expect(snapshot.entries.map((entry) => entry.sequence)).toEqual(Array.from({ length: 200 }, (_, index) => index + 6));
  snapshot = applyActivityEvent(snapshot, { type: 'message_delivered', sessionId: SESSION, sequence: 206, message: { ...MESSAGE, timestamp: 0 } });
  expect(snapshot.entries[0].sequence).toBe(6);
});

it('seeds at sequence zero, deduplicates jobs and buckets messages only in Recent', () => {
  const seed = seedActivityFromList(SESSION, [LIVE, { ...LIVE, description: 'Replacement' }, DONE, { id: '' }], [MESSAGE, { id: 'invalid' }]);
  expect(seed.sequence).toBe(0);
  expect(seed.entries.filter((entry) => activityEntryMatchesPage(entry, 'recent')).map((entry) => entry.summary)).toEqual(['Replacement', MESSAGE.body]);
  expect(seed.entries.filter((entry) => activityEntryMatchesPage(entry, 'settled')).map((entry) => entry.summary)).toEqual(['Delivered answer']);
  expect(applyActivityEvent(seed, { type: 'job_updated', sessionId: SESSION, sequence: 1, job: { ...LIVE, description: 'Live replacement' } }).entries.some((entry) => entry.summary === 'Live replacement')).toBe(true);
  expect(seedActivityFromList(SESSION, [], Array.from({ length: 205 }, (_, timestamp) => ({ ...MESSAGE, id: String(timestamp), timestamp }))).entries[0].sourceId).toBe('5');
});

it('loads session-scoped jobs, paints descriptions and closes with Back', async () => {
  const { client, calls } = mockClient();
  const close = jest.fn();
  await withPanel(client, async (tree) => {
    expect(calls).toEqual([{ command: { type: 'job_list' }, sessionId: SESSION }]);
    expect(text(tree)).toContain(LIVE.description);
    expect(text(tree)).toContain('1 event');
    await press(tree, 'Back');
    expect(close).toHaveBeenCalledTimes(1);
  }, jest.fn(), close);
});

it('filters Recent and Settled, preserves output and shows the appropriate empty copy', async () => {
  const { client } = mockClient(() => list([LIVE, DONE], [MESSAGE]));
  await withPanel(client, async (tree) => {
    expect(text(tree)).toContain('3 events');
    expect(text(tree)).toContain(MESSAGE.body);
    expect(text(tree)).not.toContain('Delivered answer');
    expect(text(tree).indexOf(MESSAGE.body)).toBeLessThan(text(tree).indexOf(LIVE.description));
    await press(tree, 'Settled');
    expect(text(tree)).toContain('Delivered answer');
    expect(text(tree)).not.toContain(MESSAGE.body);
    expect(text(tree)).not.toContain(LIVE.description);
  });
  await withPanel(mockClient(() => list([])).client, async (tree) => {
    expect(text(tree)).toContain('No ordinary activity yet.');
    await press(tree, 'Settled');
    expect(text(tree)).toContain('No settled ordinary activity yet.');
  });
});

it('upserts same-session jobs, appends mailbox frames and ignores foreign frames', async () => {
  const { client, emit } = mockClient();
  await withPanel(client, async (tree) => {
    await act(async () => {
      emit({ type: 'job_updated', sessionId: OTHER, sequence: 99, job: { ...LIVE, description: 'Foreign job' } });
      emit({ type: 'message_delivered', sessionId: OTHER, sequence: 99, message: { ...MESSAGE, body: 'Foreign message' } });
      emit({ type: 'job_updated', sessionId: SESSION, sequence: 1, job: { ...LIVE, description: 'Updated job' } });
      emit({ type: 'message_delivered', sessionId: SESSION, sequence: 2, message: MESSAGE });
      emit({ type: 'agent_updated', sessionId: SESSION, sequence: 3, agent: { id: 'a1', displayName: 'Renamed actor' } });
    });
    expect(text(tree)).toContain('2 events');
    expect(text(tree)).toContain('Updated job');
    expect(text(tree)).toContain('Renamed actor');
    expect(text(tree)).toContain(MESSAGE.body);
    expect(text(tree)).not.toContain(LIVE.description);
    expect(text(tree)).not.toContain('Foreign');
  });
});

it('replays live frames received while the initial list is pending', async () => {
  const pending = deferred();
  const { client, emit } = mockClient(() => pending.promise);
  await withPanel(client, async (tree) => {
    await act(async () => {
      emit({ type: 'job_updated', sessionId: SESSION, sequence: 1, job: { ...LIVE, description: 'During load' } });
      emit({ type: 'message_delivered', sessionId: SESSION, sequence: 2, message: MESSAGE });
      pending.resolve(list());
    });
    expect(text(tree)).toContain('During load');
    expect(text(tree)).toContain(MESSAGE.body);
    expect(text(tree)).not.toContain(LIVE.description);
  });
});

it('switches session ownership, resets the page and ignores old list and retired frame callbacks', async () => {
  const pending = deferred();
  const errors = jest.fn();
  const { client, calls, listeners, emit } = mockClient((sessionId) => sessionId === SESSION ? pending.promise : list([{ ...LIVE, description: 'Other session job' }]));
  await withPanel(client, async (tree) => {
    await press(tree, 'Settled');
    await act(async () => { tree.update(panel(client, errors, OTHER)); });
    await act(async () => {
      pending.resolve(list());
      listeners[0]({ type: 'job_updated', sessionId: SESSION, sequence: 90, job: { ...LIVE, description: 'Retired frame' } });
      emit({ type: 'job_updated', sessionId: SESSION, sequence: 99, job: LIVE });
    });
    expect(calls).toEqual([{ command: { type: 'job_list' }, sessionId: SESSION }, { command: { type: 'job_list' }, sessionId: OTHER }]);
    expect(text(tree)).toContain('Other session job');
    expect(text(tree)).not.toContain(LIVE.description);
    expect(text(tree)).not.toContain('Retired frame');
    expect(errors).not.toHaveBeenCalled();
  }, errors);
});

it('ignores late failed lists after unmount', async () => {
  const pending = deferred();
  const errors = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(panel(mockClient(() => pending.promise).client, errors)); });
  await act(async () => { tree.unmount(); });
  await act(async () => { pending.resolve({ success: false, error: 'Late failure' }); });
  expect(errors).not.toHaveBeenCalled();
});

it.each([
  { success: false, error: 'Unavailable' },
  { success: true, data: {} },
  list([LIVE], 'invalid mailbox'),
])('reports failed or malformed lists without painting their jobs', async (response) => {
  const errors = jest.fn();
  await withPanel(mockClient(() => response).client, async (tree) => {
    expect(errors).toHaveBeenCalledTimes(1);
    expect(text(tree)).toContain('No ordinary activity yet.');
    expect(text(tree)).not.toContain(LIVE.description);
  }, errors);
});

it('redacts secrets in seeded and live actors and summaries at render time', async () => {
  const raw = { ...LIVE, agent: `Worker ${SECRET}`, description: `Task ${SECRET}` };
  expect(seedActivityFromList(SESSION, [raw], []).entries[0].summary).toContain(SECRET);
  const { client, emit } = mockClient(() => list([raw], [{ ...MESSAGE, from: SECRET, body: `Seed ${SECRET}` }]));
  await withPanel(client, async (tree) => {
    expect(text(tree)).toContain('Worker [REDACTED]');
    expect(text(tree)).toContain('Task [REDACTED]');
    expect(text(tree)).toContain('[REDACTED] → parent');
    expect(text(tree)).not.toContain(SECRET);
    await act(async () => { emit({ type: 'message_delivered', sessionId: SESSION, sequence: 1, message: { ...MESSAGE, body: `Live ${SECRET}` } }); });
    expect(text(tree)).toContain('Live [REDACTED]');
    expect(text(tree)).not.toContain(SECRET);
  });
});
