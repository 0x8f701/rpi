import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Text, TextInput, TouchableOpacity } from 'react-native';
import { PersonasPanel } from '../src/screens/PersonasPanel';
import type { RunnerClient } from '../src/runnerClient';
import { DARK } from '../src/ui';

const ROW = {
  name: 'mentor',
  description: 'review partner',
  source: 'user',
  trusted: true,
  preferred: false,
  contractSummary: 'plans then reviews',
  memoryEntries: 2,
  sessionCount: 1,
  stateError: null,
};
const DETAIL = {
  ...ROW,
  content: '---\nname: mentor\ndescription: review partner\ntools: [read, grep]\n---\nmentor prompt',
  contentTruncated: false,
};
type Command = Record<string, unknown>;
const ok = (command: string, data: unknown) => ({ type: 'response', command, success: true, data });
const fail = (command: string) => ({ type: 'response', command, success: false, error: 'Backend refused request' });

async function withPanel(
  handler: (command: Command) => unknown,
  run: (tree: renderer.ReactTestRenderer, calls: Command[], errors: string[]) => Promise<void>,
) {
  const calls: Command[] = [];
  const errors: string[] = [];
  const client = {
    command: async (command: Command, sessionId: string) => {
      expect(sessionId).toBe('s1');
      calls.push(command);
      return handler(command);
    },
  } as unknown as RunnerClient;
  let tree!: renderer.ReactTestRenderer;
  try {
    await act(async () => {
      tree = renderer.create(
        <PersonasPanel palette={DARK} client={client} sessionId="s1" onClose={() => {}} onError={(error) => errors.push(error)} />,
      );
    });
    await run(tree, calls, errors);
  } finally {
    if (tree) await act(async () => tree.unmount());
  }
}

function normal(command: Command) {
  if (command.type === 'persona_list') return ok('persona_list', { enabled: true, personas: [ROW] });
  if (command.type === 'persona_current') return ok('persona_current', { name: null, message: 'No preferred persona' });
  if (command.type === 'persona_get') return ok('persona_get', DETAIL);
  return ok(String(command.type), { name: 'mentor', message: 'Saved' });
}

async function press(tree: renderer.ReactTestRenderer, label: string) {
  await act(async () => {
    const node = tree.root.findAllByType(TouchableOpacity).find((item) => item.props.accessibilityLabel === label);
    expect(node).toBeDefined();
    if (!node!.props.disabled) node!.props.onPress();
  });
}

async function typeInto(tree: renderer.ReactTestRenderer, label: string, value: string) {
  await act(async () => {
    tree.root.findAllByType(TextInput).find((node) => node.props.accessibilityLabel === label)!.props.onChangeText(value);
  });
}

function text(tree: renderer.ReactTestRenderer) {
  return tree.root.findAllByType(Text).map((node) => React.Children.toArray(node.props.children).join('')).join('\n');
}

function value(tree: renderer.ReactTestRenderer, label: string) {
  return tree.root.findAllByType(TextInput).find((node) => node.props.accessibilityLabel === label)!.props.value;
}

describe('PersonasPanel', () => {
  it('loads session personas and current selection', async () => {
    await withPanel(
      (command) => command.type === 'persona_current' ? ok('persona_current', { name: 'mentor', message: 'Selected mentor' }) : normal(command),
      async (tree) => {
        expect(text(tree)).toContain('mentor · Selected');
        expect(text(tree)).toContain('user scope');
        expect(text(tree)).toContain('review partner');
        expect(text(tree)).toContain('memory: 2 entries · sessions: 1 archive');
      },
    );
  });

  it('shows real detail and rejects malformed definitions', async () => {
    let malformed = false;
    await withPanel(
      (command) => command.type === 'persona_get' && malformed ? ok('persona_get', { name: 'mentor' }) : normal(command),
      async (tree, calls, errors) => {
        await press(tree, 'View mentor');
        expect(text(tree)).toContain('mentor prompt');
        expect(calls).toContainEqual({ type: 'persona_get', name: 'mentor' });
        malformed = true;
        await press(tree, 'View mentor');
        expect(text(tree)).not.toContain('mentor prompt');
        expect(text(tree)).toContain('unreadable definition');
        expect(errors).toHaveLength(1);
      },
    );
  });

  it('creates the entered definition and refreshes the list', async () => {
    await withPanel(normal, async (tree, calls) => {
      await press(tree, 'New persona');
      await typeInto(tree, 'Persona name', 'writer');
      await typeInto(tree, 'Persona definition', 'writer definition');
      await press(tree, 'Create persona');
      expect(calls).toContainEqual({ type: 'persona_create', name: 'writer', content: 'writer definition' });
      expect(calls.filter((command) => command.type === 'persona_list')).toHaveLength(2);
    });
  });

  it('retains a refused create draft without refreshing', async () => {
    await withPanel(
      (command) => command.type === 'persona_create' ? fail('persona_create') : normal(command),
      async (tree, calls, errors) => {
        await press(tree, 'New persona');
        await typeInto(tree, 'Persona name', 'writer');
        await typeInto(tree, 'Persona definition', 'draft');
        await press(tree, 'Create persona');
        expect(value(tree, 'Persona name')).toBe('writer');
        expect(value(tree, 'Persona definition')).toBe('draft');
        expect(errors).toEqual(['Backend refused request']);
        expect(calls.filter((command) => command.type === 'persona_list')).toHaveLength(1);
      },
    );
  });

  it('seeds editing, retains refused edits, then saves the revised definition', async () => {
    let refused = true;
    await withPanel(
      (command) => command.type === 'persona_edit' && refused ? fail('persona_edit') : normal(command),
      async (tree, calls) => {
        await press(tree, 'Edit mentor');
        expect(value(tree, 'Persona definition')).toBe(DETAIL.content);
        await typeInto(tree, 'Persona definition', 'revised');
        await press(tree, 'Save persona');
        expect(value(tree, 'Persona definition')).toBe('revised');
        refused = false;
        await press(tree, 'Save persona');
        expect(calls).toContainEqual({ type: 'persona_edit', name: 'mentor', content: 'revised' });
        expect(tree.root.findAllByType(TextInput)).toHaveLength(0);
      },
    );
  });

  it('selects and clears the backend preference', async () => {
    let current: string | null = null;
    await withPanel(
      (command) => {
        if (command.type === 'persona_select') current = String(command.name);
        if (command.type === 'persona_clear') current = null;
        return command.type === 'persona_current' ? ok('persona_current', { name: current, message: 'Preference' }) : normal(command);
      },
      async (tree, calls) => {
        await press(tree, 'Select mentor');
        expect(text(tree)).toContain('Selected');
        await press(tree, 'Clear selection');
        expect(text(tree)).not.toContain('Selected');
        expect(calls).toContainEqual({ type: 'persona_select', name: 'mentor' });
        expect(calls).toContainEqual({ type: 'persona_clear' });
      },
    );
  });

  it('requires confirmation and distinguishes keeping state from purging it', async () => {
    await withPanel(normal, async (tree, calls) => {
      await press(tree, 'Remove or purge mentor');
      expect(text(tree)).toContain('memory/ and sessions/ stay');
      expect(text(tree)).toContain('Deletes the whole persona root');
      await press(tree, 'Cancel deletion');
      expect(calls.some((command) => command.type === 'persona_remove' || command.type === 'persona_purge')).toBe(false);
      await press(tree, 'Remove or purge mentor');
      await press(tree, 'Remove definition (keep state)');
      expect(calls).toContainEqual({ type: 'persona_remove', name: 'mentor', confirm: true });
      await press(tree, 'Remove or purge mentor');
      await press(tree, 'Purge root (delete everything)');
      expect(calls).toContainEqual({ type: 'persona_purge', name: 'mentor', confirm: true });
    });
  });

  it('keeps last-good rows when refresh fails', async () => {
    let broken = false;
    await withPanel(
      (command) => command.type === 'persona_list' && broken ? fail('persona_list') : normal(command),
      async (tree, _calls, errors) => {
        broken = true;
        await press(tree, 'Refresh personas');
        expect(text(tree)).toContain('review partner');
        expect(errors).toEqual(['Backend refused request']);
      },
    );
  });

  it('ignores a detail response from an older selection', async () => {
    const delayed = Promise.withResolvers<unknown>();
    await withPanel(
      (command) => {
        if (command.type === 'persona_list') return ok('persona_list', { enabled: true, personas: [ROW, { ...ROW, name: 'writer' }] });
        if (command.type === 'persona_get' && command.name === 'mentor') return delayed.promise;
        if (command.type === 'persona_get') return ok('persona_get', { ...DETAIL, name: 'writer', content: 'writer prompt' });
        return normal(command);
      },
      async (tree) => {
        await press(tree, 'View mentor');
        await press(tree, 'View writer');
        await act(async () => delayed.resolve(ok('persona_get', DETAIL)));
        expect(text(tree)).toContain('writer prompt');
        expect(text(tree)).not.toContain('mentor prompt');
      },
    );
  });
});
