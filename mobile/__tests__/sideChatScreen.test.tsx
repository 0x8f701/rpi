import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text } from 'react-native';
import { MarkdownBody } from '../src/MarkdownBody';
import type { RunnerClient } from '../src/runnerClient';
import {
  generateSideChatName,
  SIDE_CHAT_NAME_MAX_CHARS,
  SIDE_CHAT_RESERVED_NAMES,
  SideChatScreen,
} from '../src/screens/SideChatScreen';
import { DARK } from '../src/ui';

type WireCommand = { type: string; name?: string; message?: string };

function mockClient(handler: (command: WireCommand) => unknown): RunnerClient {
  return {
    command: async (command: WireCommand) => handler(command),
    subscribeFrames: () => () => {},
  } as unknown as RunnerClient;
}

function ok(command: string, data: unknown): unknown {
  return { type: 'response', command, success: true, data };
}

function fail(command: string, error: string): unknown {
  return { type: 'response', command, success: false, error };
}

function tab(over: Record<string, unknown> = {}) {
  return {
    name: 'amber_otter',
    streaming: false,
    status: 'idle',
    entries: [],
    ...over,
  };
}

function snapshot(over: Record<string, unknown> = {}) {
  return {
    active: 'amber_otter',
    maxTabs: 8,
    tabs: [tab()],
    ...over,
  };
}

async function mount(
  client: RunnerClient,
  extras: { random?: () => number; onError?: (message: string) => void } = {},
): Promise<renderer.ReactTestRenderer> {
  const errors: string[] = [];
  const onError = extras.onError ?? ((message: string) => { errors.push(message); });
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <SideChatScreen
        palette={DARK}
        client={client}
        sessionId="s1"
        onError={onError}
        random={extras.random}
      />,
    );
  });
  await act(async () => { await Promise.resolve(); });
  return tree;
}

async function unmount(tree: renderer.ReactTestRenderer): Promise<void> {
  await act(async () => {
    tree.unmount();
  });
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  const control = tree.root.findAll((node) => node.props.accessibilityLabel === label)[0];
  expect(control).toBeTruthy();
  await act(async () => {
    control.props.onPress();
  });
  await act(async () => { await Promise.resolve(); });
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

describe('generateSideChatName', () => {
  it('is unique against existing tabs, skips reserved tokens, and stays bounded', () => {
    const first = generateSideChatName([], () => 0);
    expect(first).toBe('amber_otter');
    const second = generateSideChatName([first], () => 0);
    expect(second).toBe('amber_otter_2');
    expect(second).not.toBe(first);
    const reservedHit = generateSideChatName([...SIDE_CHAT_RESERVED_NAMES, first], () => 0);
    expect(SIDE_CHAT_RESERVED_NAMES).not.toContain(reservedHit);
    expect(reservedHit).not.toBe('/btw');
    expect(reservedHit.length).toBeLessThanOrEqual(SIDE_CHAT_NAME_MAX_CHARS);
    expect(second.length).toBeLessThanOrEqual(SIDE_CHAT_NAME_MAX_CHARS);
  });
});

describe('SideChatScreen', () => {
  it('renders a tool entry as a card and never as a raw tagged text row', async () => {
    const client = mockClient((command) => {
      if (command.type !== 'side_chat_list') return fail(command.type, 'unexpected');
      return ok('side_chat_list', snapshot({
        tabs: [tab({
          entries: [{
            id: 't1',
            kind: 'tool',
            role: 'tool',
            text: '[bash] ls',
            toolName: 'bash',
            toolArgs: { command: 'ls' },
            output: 'ok',
            thinking: null,
            isError: false,
            isPartial: false,
          }],
        })],
      }));
    });
    const tree = await mount(client);
    expect(tree.root.findByProps({ accessibilityLabel: 'Tool bash' })).toBeTruthy();
    expect(renderedStrings(tree).some((text) => text.includes('[bash]'))).toBe(false);
    await unmount(tree);
  });

  it('renders a partial assistant entry through MarkdownBody', async () => {
    const client = mockClient((command) => {
      if (command.type !== 'side_chat_list') return fail(command.type, 'unexpected');
      return ok('side_chat_list', snapshot({
        tabs: [tab({
          entries: [{
            id: 'a1',
            kind: 'assistant',
            role: 'assistant',
            text: '**live**',
            toolName: null,
            toolArgs: null,
            output: null,
            thinking: null,
            isError: false,
            isPartial: true,
          }],
        })],
      }));
    });
    const tree = await mount(client);
    const bodies = tree.root.findAllByType(MarkdownBody);
    expect(bodies.some((node) => node.props.text === '**live**')).toBe(true);
    await unmount(tree);
  });

  it('Stop sends side_chat_abort for the active tab and never a close', async () => {
    const calls: WireCommand[] = [];
    const client = mockClient((command) => {
      calls.push(command);
      if (command.type === 'side_chat_list' || command.type === 'side_chat_abort') {
        return ok(command.type, snapshot({
          tabs: [tab({ streaming: true })],
        }));
      }
      return fail(command.type, 'unexpected');
    });
    const tree = await mount(client);
    await press(tree, 'Stop');
    expect(calls.some((call) => call.type === 'side_chat_abort')).toBe(true);
    expect(calls.some((call) => call.type === 'side_chat_close')).toBe(false);
    expect(calls.filter((call) => call.type === 'side_chat_abort').every((call) => call.name == null)).toBe(true);
    await unmount(tree);
  });

  it('a rejected prompt surfaces one error and preserves the draft', async () => {
    const errors: string[] = [];
    const client = mockClient((command) => {
      if (command.type === 'side_chat_list') return ok('side_chat_list', snapshot());
      if (command.type === 'side_chat_prompt') return fail('side_chat_prompt', 'tab is streaming');
      return fail(command.type, 'unexpected');
    });
    const tree = await mount(client, { onError: (message) => { errors.push(message); } });
    const composer = tree.root.findByProps({ accessibilityLabel: 'Side chat composer' });
    await act(async () => {
      composer.props.onChangeText('keep this draft');
    });
    await press(tree, 'Send');
    expect(errors).toEqual(['tab is streaming']);
    expect(tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.value).toBe('keep this draft');
    expect(tree.root.findByProps({ accessibilityLabel: 'Side chat prompt error' }).props.children).toBe('tab is streaming');
    await unmount(tree);
  });

  it('New uses a generated name unique against current tabs', async () => {
    const calls: WireCommand[] = [];
    const client = mockClient((command) => {
      calls.push(command);
      if (command.type === 'side_chat_list') return ok('side_chat_list', snapshot());
      if (command.type === 'side_chat_new') {
        return ok('side_chat_new', snapshot({
          active: command.name,
          tabs: [tab(), tab({ name: command.name ?? '' })],
        }));
      }
      return fail(command.type, 'unexpected');
    });
    const tree = await mount(client, { random: () => 0 });
    await press(tree, 'New side-chat tab');
    const created = calls.find((call) => call.type === 'side_chat_new');
    expect(created?.name).toBe('amber_otter_2');
    expect(created?.name?.length ?? 99).toBeLessThanOrEqual(SIDE_CHAT_NAME_MAX_CHARS);
    expect(SIDE_CHAT_RESERVED_NAMES).not.toContain(created?.name);
    await unmount(tree);
  });

  it('releases composer busy when a tab switch supersedes an in-flight prompt', async () => {
    const pending = Promise.withResolvers<unknown>();
    const calls: WireCommand[] = [];
    const client = mockClient((command) => {
      calls.push(command);
      if (command.type === 'side_chat_list') {
        return ok('side_chat_list', snapshot({
          tabs: [tab(), tab({ name: 'coral_heron' })],
        }));
      }
      if (command.type === 'side_chat_prompt') return pending.promise;
      if (command.type === 'side_chat_switch') {
        return ok('side_chat_switch', snapshot({
          active: command.name,
          tabs: [tab(), tab({ name: 'coral_heron' })],
        }));
      }
      return fail(command.type, 'unexpected');
    });
    const tree = await mount(client);
    await act(async () => {
      tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.onChangeText('keep this draft');
    });
    await press(tree, 'Send');
    expect(tree.root.findByProps({ accessibilityLabel: 'Send' }).props.disabled).toBe(true);
    await press(tree, 'Switch to coral_heron');
    expect(tree.root.findByProps({ accessibilityLabel: 'Send' }).props.disabled).toBe(false);
    await act(async () => {
      pending.resolve(ok('side_chat_prompt', snapshot({
        tabs: [tab({ entries: [{ id: 'u1', kind: 'user', role: 'user', text: 'keep this draft', toolName: null, toolArgs: null, output: null, thinking: null, isError: false, isPartial: false }] })],
      })));
    });
    await act(async () => { await Promise.resolve(); });
    expect(tree.root.findByProps({ accessibilityLabel: 'Send' }).props.disabled).toBe(false);
    expect(tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.value).toBe('keep this draft');
    expect(calls.some((call) => call.type === 'side_chat_switch' && call.name === 'coral_heron')).toBe(true);
    await unmount(tree);
  });

  it('does not let a superseded prompt unlock a newer in-flight Send', async () => {
    const first = Promise.withResolvers<unknown>();
    const second = Promise.withResolvers<unknown>();
    let prompts = 0;
    const client = mockClient((command) => {
      if (command.type === 'side_chat_list') {
        return ok('side_chat_list', snapshot({
          tabs: [tab(), tab({ name: 'coral_heron' })],
        }));
      }
      if (command.type === 'side_chat_prompt') {
        prompts += 1;
        return prompts === 1 ? first.promise : second.promise;
      }
      if (command.type === 'side_chat_switch') {
        return ok('side_chat_switch', snapshot({
          active: command.name,
          tabs: [tab(), tab({ name: 'coral_heron' })],
        }));
      }
      return fail(command.type, 'unexpected');
    });
    const tree = await mount(client);
    await act(async () => {
      tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.onChangeText('first');
    });
    await press(tree, 'Send');
    await press(tree, 'Switch to coral_heron');
    await act(async () => {
      tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.onChangeText('second');
    });
    await press(tree, 'Send');
    expect(tree.root.findByProps({ accessibilityLabel: 'Send' }).props.disabled).toBe(true);
    await act(async () => {
      first.resolve(ok('side_chat_prompt', snapshot({
        tabs: [tab(), tab({ name: 'coral_heron' })],
      })));
    });
    await act(async () => { await Promise.resolve(); });
    expect(tree.root.findByProps({ accessibilityLabel: 'Send' }).props.disabled).toBe(true);
    expect(prompts).toBe(2);
    await act(async () => {
      second.resolve(ok('side_chat_prompt', snapshot({
        active: 'coral_heron',
        tabs: [tab(), tab({ name: 'coral_heron' })],
      })));
    });
    await act(async () => { await Promise.resolve(); });
    expect(tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.value).toBe('');
    await act(async () => {
      tree.root.findByProps({ accessibilityLabel: 'Side chat composer' }).props.onChangeText('third');
    });
    expect(tree.root.findByProps({ accessibilityLabel: 'Send' }).props.disabled).toBe(false);
    await unmount(tree);
  });

  it('malformed snapshot shows an empty state and no fabricated rows', async () => {
    const client = mockClient((command) => {
      if (command.type !== 'side_chat_list') return fail(command.type, 'unexpected');
      return ok('side_chat_list', { active: 1, tabs: 'nope' });
    });
    const tree = await mount(client);
    expect(renderedStrings(tree).some((text) => text.includes('unreadable'))).toBe(true);
    expect(tree.root.findAll((node) => node.props.accessibilityLabel === 'Tool bash')).toHaveLength(0);
    await unmount(tree);
  });
});
