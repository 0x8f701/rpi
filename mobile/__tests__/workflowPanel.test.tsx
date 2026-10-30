import React from 'react';
import { Text } from 'react-native';
import renderer, { act } from 'react-test-renderer';
import type { RunnerCommand } from '../src/protocol';
import type { RunnerClient } from '../src/runnerClient';
import { WorkflowPanel } from '../src/screens/WorkflowPanel';
import { DARK } from '../src/ui';

/** Realistic `workflow_list` / lifecycle row (workflow_rpc::project_workflow_snapshot). */
function row(over: Record<string, unknown> = {}) {
  return {
    workflowId: 'wf-alpha',
    name: 'Alpha',
    objective: 'Ship the alpha slice',
    status: 'running',
    generation: 3,
    worktree: 'wf-alpha',
    branch: 'workflow/wf-alpha',
    supervisorAgentId: 'Supervisor',
    supervisorJobId: 'job-sup',
    failure: null,
    integration: null,
    ...over,
  };
}

/** Realistic `workflow_detail` actor (workflow_panel::WorkflowActorSnapshot). */
function actor(over: Record<string, unknown> = {}) {
  return {
    name: 'Supervisor',
    role: 'supervisor',
    status: 'running',
    task: 'Coordinate workers',
    taskId: null,
    activity: [],
    ...over,
  };
}

/** Realistic live `workflow_detail` projection (camelCase, snake_case enums). */
function detail(over: Record<string, unknown> = {}) {
  return {
    id: 'wf-alpha',
    sessionId: '',
    generation: 3,
    name: 'Alpha',
    objective: 'Ship the alpha slice',
    status: 'running',
    failure: null,
    supervisor: actor(),
    subagents: [actor({
      name: 'Worker',
      role: 'task',
      status: 'running',
      task: 'Implement the slice',
      activity: [
        { atMs: 1000, kind: 'task', text: 'job started' },
        { atMs: 2000, kind: 'irc', text: 'ping worker' },
      ],
    })],
    jobs: [{
      displayName: 'Worker',
      jobId: 'job-1',
      role: 'task',
      status: 'completed',
      taskSummary: 'Implement the slice',
      todoTaskId: 't1',
      createdAtMs: 900,
      startedAtMs: 1000,
      finishedAtMs: 3000,
      softBudgetExhausted: false,
    }],
    activeTasks: [{ taskId: 't1', summary: 'Implement the slice' }],
    worktree: { label: 'wf-alpha', branch: 'workflow/wf-alpha' },
    integration: { step: 'pending', summary: '', filesChanged: 0, insertions: 0, deletions: 0, conflicts: [] },
    planningActivity: [{ atMs: 500, kind: 'thinking', text: 'drafting the plan' }],
    planningStartedAtMs: 400,
    recentIrc: [{
      sender: 'Supervisor',
      to: 'Worker',
      text: 'Start with the parser',
      atMs: 700,
      messageId: 'm-1',
      replyTo: null,
    }],
    ...over,
  };
}

interface Harness {
  client: RunnerClient;
  sent: Array<{ command: RunnerCommand; sessionId?: string }>;
  errors: string[];
  listeners: Array<(frame: unknown) => void>;
}

function harness(handler: (command: RunnerCommand) => unknown): Harness {
  const sent: Harness['sent'] = [];
  const errors: string[] = [];
  const listeners: Harness['listeners'] = [];
  const client = {
    command: async (command: RunnerCommand, sessionId?: string) => {
      sent.push({ command, sessionId });
      return handler(command);
    },
    subscribeFrames: (listener: (frame: unknown) => void) => {
      listeners.push(listener);
      return () => {};
    },
  } as unknown as RunnerClient;
  return { client, sent, errors, listeners };
}

function ok(command: RunnerCommand, data: unknown) {
  return { type: 'response', command: command.type, success: true, data };
}

function fail(command: RunnerCommand, error: string) {
  return { type: 'response', command: command.type, success: false, error };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((settle) => {
    resolve = settle;
  });
  return { promise, resolve };
}

async function flush(): Promise<void> {
  for (let tick = 0; tick < 4; tick += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

async function mount(harnessed: Harness): Promise<renderer.ReactTestRenderer> {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <WorkflowPanel
        palette={DARK}
        client={harnessed.client}
        sessionId="s1"
        onClose={() => {}}
        onError={(message) => harnessed.errors.push(message)}
      />,
    );
  });
  await flush();
  await flush();
  return tree;
}

function control(tree: renderer.ReactTestRenderer, label: string) {
  const found = tree.root.findAll((node) => node.props.accessibilityLabel === label)[0];
  expect(found).toBeTruthy();
  return found;
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  const found = control(tree, label);
  await act(async () => {
    found.props.onPress();
  });
  await flush();
}

async function type(tree: renderer.ReactTestRenderer, label: string, value: string): Promise<void> {
  const found = control(tree, label);
  await act(async () => {
    found.props.onChangeText(value);
  });
}

function renderedStrings(tree: renderer.ReactTestRenderer): string[] {
  const out: string[] = [];
  const walk = (value: unknown) => {
    if (typeof value === 'string') out.push(value);
    else if (Array.isArray(value)) value.forEach(walk);
  };
  tree.root.findAllByType(Text).forEach((node) => walk(node.props.children));
  return out;
}

function sentTypes(harnessed: Harness): string[] {
  return harnessed.sent.map((entry) => entry.command.type);
}

function lastCommand(harnessed: Harness, type: string): RunnerCommand | undefined {
  return [...harnessed.sent].reverse().find((entry) => entry.command.type === type)?.command;
}

async function unmount(tree: renderer.ReactTestRenderer): Promise<void> {
  await act(async () => {
    tree.unmount();
  });
}

describe('WorkflowPanel', () => {
  it('lists workflows and renders the selected workflow detail', async () => {
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row()] });
      if (command.type === 'workflow_detail') return ok(command, detail());
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    expect(sentTypes(harnessed)).toEqual(['workflow_list', 'workflow_detail']);
    expect(harnessed.sent.every((entry) => entry.sessionId === 's1')).toBe(true);
    expect(lastCommand(harnessed, 'workflow_detail')).toMatchObject({ workflowId: 'wf-alpha' });

    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('Alpha'))).toBe(true);
    expect(texts.some((text) => text.includes('Implement the slice'))).toBe(true);
    expect(texts.some((text) => text.includes('Start with the parser'))).toBe(true);
    expect(texts.some((text) => text.includes('drafting the plan'))).toBe(true);
    expect(texts.some((text) => text.includes('Worker') && text.includes('2s'))).toBe(true);
    expect(harnessed.errors).toEqual([]);
    await unmount(tree);
  });

  it('creates a workflow with the typed name and objective, then selects it', async () => {
    let created: Record<string, unknown> | null = null;
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: created === null ? [] : [created] });
      if (command.type === 'workflow_create') {
        created = row({ workflowId: 'wf-new', name: '  Beta  ', objective: '  Second slice  ', generation: 1 });
        return ok(command, created);
      }
      if (command.type === 'workflow_detail') return ok(command, detail({ id: 'wf-new', name: 'Beta', generation: 1 }));
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    await type(tree, 'Workflow name', '  Beta  ');
    await type(tree, 'Workflow objective', '  Second slice  ');
    await press(tree, 'Create workflow');

    expect(lastCommand(harnessed, 'workflow_create')).toEqual({
      type: 'workflow_create',
      name: 'Beta',
      objective: 'Second slice',
    });
    expect(lastCommand(harnessed, 'workflow_detail')).toMatchObject({ workflowId: 'wf-new' });
    expect(renderedStrings(tree).some((text) => text.includes('Beta'))).toBe(true);
    await unmount(tree);
  });

  it('derives action availability from the typed status', async () => {
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row({ status: 'completed' })] });
      if (command.type === 'workflow_detail') return ok(command, detail({ status: 'completed' }));
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    expect(control(tree, 'Pause Alpha').props.disabled).toBe(true);
    expect(control(tree, 'Resume Alpha').props.disabled).toBe(true);
    expect(control(tree, 'Cancel Alpha').props.disabled).toBe(true);
    expect(control(tree, 'Integrate Alpha').props.disabled).toBe(false);
    expect(control(tree, 'Remove Alpha').props.disabled).toBe(false);
    await unmount(tree);
  });

  it('sends the lifecycle command for the selected workflow', async () => {
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') {
        const paused = sentTypes(harnessed).includes('workflow_pause');
        return ok(command, { workflows: [row({ status: paused ? 'paused' : 'running' })] });
      }
      if (command.type === 'workflow_pause') return ok(command, row({ status: 'paused' }));
      if (command.type === 'workflow_detail') {
        const paused = sentTypes(harnessed).includes('workflow_pause');
        return ok(command, detail({ status: paused ? 'paused' : 'running' }));
      }
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    expect(control(tree, 'Integrate Alpha').props.disabled).toBe(true);
    await press(tree, 'Pause Alpha');

    expect(lastCommand(harnessed, 'workflow_pause')).toEqual({ type: 'workflow_pause', workflowId: 'wf-alpha' });
    expect(harnessed.errors).toEqual([]);
    expect(control(tree, 'Resume Alpha').props.disabled).toBe(false);
    await unmount(tree);
  });

  it('reports a lifecycle rejection once and keeps the last-good detail', async () => {
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row()] });
      if (command.type === 'workflow_pause') return fail(command, 'workflow lifecycle transition is not allowed');
      if (command.type === 'workflow_detail') return ok(command, detail());
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    await press(tree, 'Pause Alpha');

    expect(harnessed.errors).toHaveLength(1);
    expect(harnessed.errors[0]).toContain('workflow lifecycle transition is not allowed');
    // The rejection never flips the rendered status, and the last-good detail
    // (planning activity, IRC, jobs) stays rendered.
    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('Start with the parser'))).toBe(true);
    expect(texts.some((text) => text.includes('drafting the plan'))).toBe(true);
    expect(control(tree, 'Pause Alpha').props.disabled).toBe(false);
    expect(control(tree, 'Integrate Alpha').props.disabled).toBe(true);
    await unmount(tree);
  });

  it('requires a confirmation tap before removing a workflow', async () => {
    let removed = false;
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') {
        return ok(command, { workflows: removed ? [] : [row()] });
      }
      if (command.type === 'workflow_remove') {
        removed = true;
        return ok(command, row());
      }
      if (command.type === 'workflow_detail') return ok(command, detail());
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    await press(tree, 'Remove Alpha');
    expect(sentTypes(harnessed)).not.toContain('workflow_remove');

    await press(tree, 'Confirm remove Alpha');
    expect(lastCommand(harnessed, 'workflow_remove')).toEqual({ type: 'workflow_remove', workflowId: 'wf-alpha' });
    const detailRequestsAfterRemove = harnessed.sent
      .filter((entry) => entry.command.type === 'workflow_detail').length;
    await flush();
    expect(harnessed.sent.filter((entry) => entry.command.type === 'workflow_detail').length)
      .toBe(detailRequestsAfterRemove);
    expect(renderedStrings(tree).some((text) => text.includes('No workflows yet'))).toBe(true);
    await unmount(tree);
  });

  it('sends a hub message to a recipient owned by the selected detail', async () => {
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row()] });
      if (command.type === 'workflow_detail') return ok(command, detail());
      if (command.type === 'workflow_hub_send') {
        return ok(command, {
          workflowId: 'wf-alpha',
          generation: 3,
          recipient: 'Worker',
          receipts: [{ to: 'Worker', requested: 'Worker', outcome: 'woken', messageId: 'msg-1', error: null }],
        });
      }
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    // Only the detail's own participants are addressable.
    expect(control(tree, 'Message Worker')).toBeTruthy();
    expect(control(tree, 'Message Supervisor')).toBeTruthy();
    await press(tree, 'Message Worker');
    await type(tree, 'Workflow message body', 'please review');
    await press(tree, 'Send workflow message');

    expect(lastCommand(harnessed, 'workflow_hub_send')).toEqual({
      type: 'workflow_hub_send',
      workflowId: 'wf-alpha',
      generation: 3,
      recipient: 'Worker',
      body: 'please review',
    });
    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('woken'))).toBe(true);
    expect(texts.some((text) => text.includes('msg-1'))).toBe(true);
    expect(control(tree, 'Workflow message body').props.value).toBe('');
    expect(harnessed.errors).toEqual([]);
    await unmount(tree);
  });

  it('rejects receipts that answer for a different recipient', async () => {
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row()] });
      if (command.type === 'workflow_detail') return ok(command, detail());
      if (command.type === 'workflow_hub_send') {
        return ok(command, {
          workflowId: 'wf-alpha',
          generation: 3,
          recipient: 'Ghost',
          receipts: [{ to: 'Ghost', outcome: 'processed', messageId: 'msg-9', error: null }],
        });
      }
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    await press(tree, 'Message Worker');
    await type(tree, 'Workflow message body', 'please review');
    await press(tree, 'Send workflow message');

    expect(harnessed.errors).toHaveLength(1);
    expect(harnessed.errors[0]).toContain('recipient mismatch');
    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('Ghost'))).toBe(false);
    expect(texts.some((text) => text.includes('msg-9'))).toBe(false);
    // A rejected send keeps the draft instead of pretending it was delivered.
    expect(control(tree, 'Workflow message body').props.value).toBe('please review');
    await unmount(tree);
  });

  it('drops a late hub-send response after the selection changed', async () => {
    const pending = deferred<unknown>();
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') {
        return ok(command, {
          workflows: [row(), row({ workflowId: 'wf-beta', name: 'Beta', objective: 'Ship the beta slice' })],
        });
      }
      if (command.type === 'workflow_detail') {
        return ok(command, detail(command.workflowId === 'wf-beta'
          ? { id: 'wf-beta', name: 'Beta', objective: 'Ship the beta slice' }
          : {}));
      }
      if (command.type === 'workflow_hub_send') return pending.promise;
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    await press(tree, 'Select workflow Beta');
    await press(tree, 'Message Supervisor');
    await type(tree, 'Workflow message body', 'stale body');
    await press(tree, 'Send workflow message');

    await press(tree, 'Select workflow Alpha');
    await act(async () => {
      pending.resolve({
        type: 'response',
        command: 'workflow_hub_send',
        success: true,
        data: {
          workflowId: 'wf-beta',
          generation: 3,
          recipient: 'Supervisor',
          receipts: [{ to: 'Supervisor', outcome: 'processed', messageId: 'msg-stale', error: null }],
        },
      });
    });
    await flush();

    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('msg-stale'))).toBe(false);
    expect(harnessed.errors).toEqual([]);
    await unmount(tree);
  });

  it('never lets a late detail response replace the current selection', async () => {
    const alpha = deferred<unknown>();
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') {
        return ok(command, {
          workflows: [row(), row({ workflowId: 'wf-beta', name: 'Beta', objective: 'Ship the beta slice' })],
        });
      }
      if (command.type === 'workflow_detail') {
        if (command.workflowId === 'wf-alpha') return alpha.promise;
        return ok(command, detail({
          id: 'wf-beta',
          name: 'Beta',
          objective: 'Ship the beta slice',
          recentIrc: [{
            sender: 'Supervisor',
            to: 'Worker',
            text: 'beta-only message',
            atMs: 700,
            messageId: 'm-beta',
            replyTo: null,
          }],
        }));
      }
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    await press(tree, 'Select workflow Beta');
    expect(renderedStrings(tree).some((text) => text.includes('beta-only message'))).toBe(true);

    await act(async () => {
      alpha.resolve({
        type: 'response',
        command: 'workflow_detail',
        success: true,
        data: detail({
          recentIrc: [{
            sender: 'Supervisor',
            to: 'Worker',
            text: 'alpha-only message',
            atMs: 700,
            messageId: 'm-alpha',
            replyTo: null,
          }],
        }),
      });
    });
    await flush();

    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('alpha-only message'))).toBe(false);
    expect(texts.some((text) => text.includes('beta-only message'))).toBe(true);
    await unmount(tree);
  });

  it('preserves the last-good detail when a refresh fails', async () => {
    jest.useFakeTimers();
    let detailCalls = 0;
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row()] });
      if (command.type === 'workflow_detail') {
        detailCalls += 1;
        return detailCalls === 1 ? ok(command, detail()) : fail(command, 'workflow detail unavailable');
      }
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);
    expect(renderedStrings(tree).some((text) => text.includes('Start with the parser'))).toBe(true);

    await act(async () => {
      jest.advanceTimersByTime(1500);
    });
    await flush();

    expect(detailCalls).toBeGreaterThan(1);
    expect(harnessed.errors).toHaveLength(1);
    expect(harnessed.errors[0]).toContain('workflow detail unavailable');
    expect(renderedStrings(tree).some((text) => text.includes('Start with the parser'))).toBe(true);
    await unmount(tree);
    jest.useRealTimers();
  });

  it('redacts and bounds model-derived detail text', async () => {
    const long = 'x'.repeat(400);
    const harnessed = harness((command) => {
      if (command.type === 'workflow_list') return ok(command, { workflows: [row()] });
      if (command.type === 'workflow_detail') {
        return ok(command, detail({
          planningActivity: [{ atMs: 500, kind: 'thinking', text: long }],
          recentIrc: [{
            sender: 'Supervisor',
            to: 'Worker',
            text: 'token sk-abcdefghijklmnop1234 leaked',
            atMs: 700,
            messageId: 'm-1',
            replyTo: null,
          }],
        }));
      }
      return fail(command, 'unexpected');
    });
    const tree = await mount(harnessed);

    const texts = renderedStrings(tree);
    expect(texts.some((text) => text.includes('[REDACTED]'))).toBe(true);
    expect(texts.some((text) => text.includes('sk-abcdefghijklmnop1234'))).toBe(false);
    expect(texts.some((text) => text.includes('x'.repeat(240)))).toBe(true);
    expect(texts.some((text) => text.includes('x'.repeat(241)))).toBe(false);
    await unmount(tree);
  });
});
