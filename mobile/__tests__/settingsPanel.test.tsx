import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { TextInput, TouchableOpacity } from 'react-native';
import { SettingsPanel } from '../src/screens/SettingsPanel';
import type { RunnerClient } from '../src/runnerClient';
import { DARK } from '../src/ui';

type Command = Record<string, unknown>;
function row(key = 'theme', kind = 'string', value: unknown = 'dark', extra: Command = {}) {
  return { definition: { key, category: 'terminalUi', valueType: { kind }, defaultJson: 'null', description: `${key} description`, enumValues: [], scopes: 'globalAndProject', behavior: 'live', secret: false, trustSensitive: false }, effectiveValue: value, globalValue: value, projectValue: null, sessionOverrideValue: null, source: 'global', editableGlobal: true, editableProject: true, redacted: false, ...extra };
}
function fixture(values = [row()], trusted = true) {
  const calls: Array<{ command: Command; sessionId: string }> = [];
  let scope = 'global';
  let staged = values;
  let override: ((command: Command, sessionId: string) => unknown) | undefined;
  const snapshot = () => ({ draftId: `draft-${scope}`, scope, dirty: true, values: staged });
  const client = { command: async (command: Command, sessionId: string) => {
    calls.push({ command, sessionId });
    const response = override?.(command, sessionId);
    if (response !== undefined) return response;
    let data: unknown = {};
    if (command.type === 'settings_inspect') data = { projectTrusted: trusted, globalPath: '/global/settings.json', projectPath: '/project/settings.json', values };
    if (command.type === 'settings_open_draft') { scope = String(command.scope); data = snapshot(); }
    if (command.type === 'settings_set') {
      staged = staged.map(view => view.definition.key === command.key ? { ...view, effectiveValue: command.value, [scope === 'global' ? 'globalValue' : 'projectValue']: command.value } : view);
      data = snapshot();
    }
    if (command.type === 'settings_reset') { staged = staged.map(view => view.definition.key === command.key ? { ...view, effectiveValue: 'default', globalValue: null, projectValue: null } : view); data = snapshot(); }
    if (command.type === 'settings_validate') data = snapshot();
    if (command.type === 'settings_apply') data = { appliedLive: true, reloaded: true, restartRequired: true, writes: [{ key: 'theme', needsRestart: false, needsReload: false }, { key: 'sessionImportSources', needsRestart: false, needsReload: true }, { key: 'approvalMode', needsRestart: true, needsReload: false }] };
    if (command.type === 'settings_cancel') data = { cancelled: true };
    return { success: true, data };
  } } as unknown as RunnerClient;
  return { client, calls, snapshot, override: (handler: typeof override) => { override = handler; } };
}
async function mounted(f: { client: RunnerClient }, run: (tree: renderer.ReactTestRenderer, errors: string[]) => Promise<void>) {
  let tree!: renderer.ReactTestRenderer;
  const errors: string[] = [];
  await act(async () => { tree = renderer.create(<SettingsPanel palette={DARK} client={f.client} sessionId="s1" onClose={() => {}} onError={message => errors.push(message)} />); });
  try { await run(tree, errors); } finally { await act(async () => tree.unmount()); }
}
async function press(tree: renderer.ReactTestRenderer, label: string) {
  await act(async () => { tree.root.findAllByType(TouchableOpacity).find(node => node.props.accessibilityLabel === label)!.props.onPress(); });
}
async function type(tree: renderer.ReactTestRenderer, label: string, value: string) {
  await act(async () => { tree.root.findAllByType(TextInput).find(node => node.props.accessibilityLabel === label)!.props.onChangeText(value); });
}
function input(tree: renderer.ReactTestRenderer, key: string) {
  return tree.root.findAllByType(TextInput).find(node => node.props.accessibilityLabel === `Value ${key}`)!;
}

it('inspects and searches, stages typed values, resets, validates, applies and reports reload/restart', async () => {
  const f = fixture([row(), row('limit', 'unsignedInteger', 3)]);
  await mounted(f, async tree => {
    await type(tree, 'Search settings', 'limit');
    expect(JSON.stringify(tree.toJSON())).not.toContain('theme description');
    await type(tree, 'Search settings', '');
    await press(tree, 'Open settings draft');
    await type(tree, 'Value limit', '5');
    await press(tree, 'Set limit');
    expect(f.calls.some(call => call.command.type === 'settings_set' && call.command.value === 5)).toBe(true);
    await press(tree, 'Reset limit');
    await type(tree, 'Value theme', 'light');
    await press(tree, 'Validate settings');
    expect(f.calls.some(call => call.command.type === 'settings_validate')).toBe(true);
    await press(tree, 'Apply settings');
    expect(JSON.stringify(tree.toJSON())).toContain('Restart required');
    expect(JSON.stringify(tree.toJSON())).toContain('Reloaded');
    expect(f.calls.every(call => call.sessionId === 's1')).toBe(true);
    expect(f.calls.find(call => call.command.type === 'settings_open_draft')?.command.scope).toBe('global');
    expect(f.calls.find(call => call.command.type === 'settings_apply')?.command.draftId).toBe('draft-global');
  });
});

it('opens project drafts and cancels before changing scope', async () => {
  const f = fixture();
  await mounted(f, async tree => {
    await press(tree, 'Project scope'); await press(tree, 'Open settings draft');
    await type(tree, 'Value theme', 'project theme');
    await press(tree, 'Cancel settings draft');
    expect(f.calls.find(call => call.command.type === 'settings_cancel')?.command.draftId).toBe('draft-project');
    await press(tree, 'Global scope'); await press(tree, 'Open settings draft');
    expect(input(tree, 'theme').props.value).toBe('dark');
  });
});

it('preserves unsaved input and last-good draft on set, reset, validate and apply refusals', async () => {
  const f = fixture();
  await mounted(f, async (tree, errors) => {
    await press(tree, 'Open settings draft'); await type(tree, 'Value theme', 'keep me');
    f.override(command => command.type === 'settings_set' || command.type === 'settings_reset' ? { success: false, error: 'write refused' } : undefined);
    await press(tree, 'Set theme'); await press(tree, 'Reset theme');
    expect(input(tree, 'theme').props.value).toBe('keep me');
    f.override(command => command.type === 'settings_validate' || command.type === 'settings_apply' ? { success: false, error: 'stale draft refused' } : undefined);
    await press(tree, 'Validate settings'); await press(tree, 'Apply settings');
    expect(input(tree, 'theme').props.value).toBe('keep me');
    expect(errors.join(' ')).toContain('settings_set');
    expect(errors.join(' ')).toContain('settings_reset');
    expect(errors.join(' ')).toContain('settings_validate');
    expect(errors.join(' ')).toContain('settings_apply');
  });
});

it('discards old owner replies and never lets an old draft overwrite the new session', async () => {
  const f = fixture();
  let resolve!: (value: unknown) => void;
  await mounted(f, async (tree, errors) => {
    f.override((command, sessionId) => command.type === 'settings_open_draft' && sessionId === 's1' ? new Promise(done => { resolve = done; }) : undefined);
    await press(tree, 'Open settings draft');
    await act(async () => { tree.update(<SettingsPanel palette={DARK} client={f.client} sessionId="s2" onClose={() => {}} onError={message => errors.push(message)} />); });
    await press(tree, 'Project scope'); await press(tree, 'Open settings draft');
    await act(async () => resolve({ success: true, data: { draftId: 'old-owner', scope: 'global', dirty: true, values: [row('old-owner')] } }));
    expect(JSON.stringify(tree.toJSON())).not.toContain('old-owner');
    await press(tree, 'Cancel settings draft');
    expect(f.calls.filter(call => call.command.type === 'settings_cancel')).toEqual([
      { sessionId: 's1', command: { type: 'settings_cancel', draftId: 'old-owner' } },
      { sessionId: 's2', command: { type: 'settings_cancel', draftId: 'draft-project' } },
    ]);
  });
});

it('keeps secret and nested-redacted editors blank and never reposts placeholders', async () => {
  const secret = row('credential', 'secret', '[redacted]', { editableGlobal: false, editableProject: false, redacted: true });
  const nested = row('service', 'object', { token: '[redacted]' });
  const f = fixture([secret, nested]);
  await mounted(f, async tree => {
    await press(tree, 'Open settings draft');
    expect(input(tree, 'credential').props.value).toBe('');
    expect(input(tree, 'credential').props.secureTextEntry).toBe(true);
    expect(input(tree, 'credential').props.editable).toBe(false);
    expect(input(tree, 'service').props.value).toBe('');
    expect(input(tree, 'service').props.secureTextEntry).toBe(true);
    await type(tree, 'Value service', '{"token":"[redacted]"}'); await press(tree, 'Set service');
    expect(f.calls.some(call => call.command.type === 'settings_set')).toBe(false);
    await type(tree, 'Value service', '{"endpoint":"https://example.invalid"}'); await press(tree, 'Apply settings');
    expect(f.calls.filter(call => call.command.type === 'settings_set').map(call => call.command.value)).toEqual([{ endpoint: 'https://example.invalid' }]);
    expect(JSON.stringify(f.calls)).not.toContain('[redacted]');
  });
});

it('fails closed for unknown types and untrusted project controls', async () => {
  const f = fixture([row('future', 'futureType'), row('trusted', 'string', 'safe', { definition: { ...row().definition, key: 'trusted', trustSensitive: true } })], false);
  await mounted(f, async tree => {
    await press(tree, 'Open settings draft');
    expect(input(tree, 'future')).toBeUndefined();
    expect(JSON.stringify(tree.toJSON())).toContain('Unsupported setting type');
    await press(tree, 'Cancel settings draft'); await press(tree, 'Project scope'); await press(tree, 'Open settings draft');
    expect(f.calls.filter(call => call.command.type === 'settings_open_draft')).toHaveLength(1);
    expect(JSON.stringify(tree.toJSON())).toContain('Project settings are read-only');
  });
});

it('serializes apply and cancel even when handlers are invoked in the same turn', async () => {
  const f = fixture();
  let resolve!: (value: unknown) => void;
  await mounted(f, async tree => {
    await press(tree, 'Open settings draft');
    f.override(command => command.type === 'settings_apply' ? new Promise(done => { resolve = done; }) : undefined);
    await act(async () => {
      const buttons = tree.root.findAllByType(TouchableOpacity);
      buttons.find(node => node.props.accessibilityLabel === 'Apply settings')!.props.onPress();
      buttons.find(node => node.props.accessibilityLabel === 'Apply settings')!.props.onPress();
      buttons.find(node => node.props.accessibilityLabel === 'Cancel settings draft')!.props.onPress();
    });
    expect(f.calls.filter(call => call.command.type === 'settings_apply')).toHaveLength(1);
    expect(f.calls.filter(call => call.command.type === 'settings_cancel')).toHaveLength(0);
    await act(async () => resolve({ success: true, data: { appliedLive: true, reloaded: false, restartRequired: false, writes: [] } }));
  });
});

it('retains the last-good catalog and draft after malformed replies', async () => {
  const f = fixture();
  await mounted(f, async (tree, errors) => {
    f.override(command => command.type === 'settings_inspect' ? { success: true, data: { values: 'invalid' } } : undefined);
    await press(tree, 'Refresh settings');
    expect(JSON.stringify(tree.toJSON())).toContain('theme description');
    f.override(undefined);
    await press(tree, 'Open settings draft'); await type(tree, 'Value theme', 'preserved');
    f.override(command => command.type === 'settings_set' ? { success: true, data: { ...f.snapshot(), draftId: 'foreign-draft' } } : undefined);
    await press(tree, 'Set theme');
    expect(input(tree, 'theme').props.value).toBe('preserved');
    expect(errors.join(' ')).toContain('Malformed settings catalog');
    expect(errors.join(' ')).toContain('mismatched settings draft');
  });
});

it('rejects invalid typed values without sending a mutation', async () => {
  const f = fixture([row('count', 'unsignedInteger', 1), row('items', 'stringList', []), row('object', 'object', {})]);
  await mounted(f, async (tree, errors) => {
    await press(tree, 'Open settings draft');
    await type(tree, 'Value count', '-1'); await press(tree, 'Set count');
    await type(tree, 'Value items', '[1]'); await press(tree, 'Set items');
    await type(tree, 'Value object', '[]'); await press(tree, 'Set object');
    expect(f.calls.some(call => call.command.type === 'settings_set')).toBe(false);
    expect(errors).toHaveLength(3);
  });
});

it('cancels the exact live server draft once on unmount', async () => {
  const f = fixture();
  await mounted(f, async tree => { await press(tree, 'Project scope'); await press(tree, 'Open settings draft'); });
  expect(f.calls.filter(call => call.command.type === 'settings_cancel')).toEqual([
    { sessionId: 's1', command: { type: 'settings_cancel', draftId: 'draft-project' } },
  ]);
});

it('does not cancel a consumed draft after successful apply', async () => {
  const f = fixture();
  await mounted(f, async tree => { await press(tree, 'Open settings draft'); await press(tree, 'Apply settings'); });
  expect(f.calls.filter(call => call.command.type === 'settings_cancel')).toEqual([]);
});

it('reports cleanup refusal centrally without updating an unmounted panel', async () => {
  const f = fixture();
  const errors: string[] = [];
  let tree!: renderer.ReactTestRenderer;
  await act(async () => { tree = renderer.create(<SettingsPanel palette={DARK} client={f.client} sessionId="s1" onClose={() => {}} onError={message => errors.push(message)} />); });
  await press(tree, 'Open settings draft');
  f.override(command => command.type === 'settings_cancel' ? { success: false, error: 'cleanup refused' } : undefined);
  await act(async () => tree.unmount());
  expect(errors).toEqual(['Clean up settings draft (s1): cleanup refused']);
});

it('retains the draft and input on a malformed cancel acknowledgement', async () => {
  const f = fixture();
  await mounted(f, async (tree, errors) => {
    await press(tree, 'Open settings draft'); await type(tree, 'Value theme', 'unsaved');
    f.override(command => command.type === 'settings_cancel' ? { success: true, data: {} } : undefined);
    await press(tree, 'Cancel settings draft');
    expect(input(tree, 'theme').props.value).toBe('unsaved');
    expect(errors).toEqual(['Cancel settings: Malformed settings_cancel acknowledgement']);
    f.override(undefined);
    await press(tree, 'Cancel settings draft');
    expect(input(tree, 'theme')).toBeUndefined();
  });
});

it('waits for an in-flight apply before deciding whether unmount needs cancellation', async () => {
  const f = fixture();
  let resolve!: (value: unknown) => void;
  await mounted(f, async tree => {
    await press(tree, 'Open settings draft');
    f.override(command => command.type === 'settings_apply' ? new Promise(done => { resolve = done; }) : undefined);
    await press(tree, 'Apply settings');
  });
  expect(f.calls.some(call => call.command.type === 'settings_cancel')).toBe(false);
  await act(async () => resolve({ success: true, data: { appliedLive: true, reloaded: false, restartRequired: false, writes: [] } }));
  expect(f.calls.some(call => call.command.type === 'settings_cancel')).toBe(false);
});

it('cancels a malformed open snapshot before permitting a replacement draft', async () => {
  const f = fixture();
  await mounted(f, async tree => {
    f.override(command => command.type === 'settings_open_draft' ? { success: true, data: { draftId: 'rejected-open', scope: 'global', dirty: false, values: 'bad' } } : undefined);
    await press(tree, 'Open settings draft');
    f.override(command => command.type === 'settings_cancel' ? { success: false, error: 'cannot release old draft' } : undefined);
    await press(tree, 'Open settings draft');
    expect(f.calls.filter(call => call.command.type === 'settings_open_draft')).toHaveLength(1);
    f.override(undefined);
    await press(tree, 'Open settings draft');
    const transitions = f.calls.filter(call => call.command.type === 'settings_open_draft' || call.command.type === 'settings_cancel');
    expect(transitions.map(call => call.command)).toEqual([
      { type: 'settings_open_draft', scope: 'global' },
      { type: 'settings_cancel', draftId: 'rejected-open' },
      { type: 'settings_cancel', draftId: 'rejected-open' },
      { type: 'settings_open_draft', scope: 'global' },
    ]);
    expect(input(tree, 'theme').props.value).toBe('dark');
  });
});

it('preserves staged draft and input after a malformed apply outcome', async () => {
  const f = fixture();
  await mounted(f, async (tree, errors) => {
    await press(tree, 'Open settings draft'); await type(tree, 'Value theme', 'retained theme');
    f.override(command => command.type === 'settings_apply' ? { success: true, data: {} } : undefined);
    await press(tree, 'Apply settings');
    expect(input(tree, 'theme').props.value).toBe('retained theme');
    expect(errors).toEqual(['Apply settings: Malformed settings apply outcome']);
    f.override(undefined);
    await press(tree, 'Cancel settings draft');
    expect(f.calls.filter(call => call.command.type === 'settings_cancel')).toEqual([
      { sessionId: 's1', command: { type: 'settings_cancel', draftId: 'draft-global' } },
    ]);
  });
});
