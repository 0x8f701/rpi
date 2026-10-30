import React, { useEffect, useRef, useState } from 'react';
import { ScrollView, Switch, Text, TextInput, TouchableOpacity, View } from 'react-native';
import { parseSettingsApplyOutcome, parseSettingsCatalog, parseSettingsDraft, type RunnerCommand, type SettingsCatalogData, type SettingsDraftData, type SettingValueViewData } from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

type Scope = 'global' | 'project';
type Input = { raw: string; dirty: boolean };
type DraftOwner = { client: RunnerClient; sessionId: string; draftId: string | null; pending: number; retired: boolean; onError: (message: string) => void };

async function releaseDraft(owner: DraftOwner): Promise<void> {
  if (!owner.retired || owner.pending || !owner.draftId) return;
  const draftId = owner.draftId;
  owner.draftId = null;
  try {
    const response = await owner.client.command({ type: 'settings_cancel', draftId }, owner.sessionId);
    if (!response.success) throw new Error(response.error || 'Request refused');
    if (!response.data || typeof response.data !== 'object' || !('cancelled' in response.data) || response.data.cancelled !== true) throw new Error('Malformed settings_cancel acknowledgement');
  } catch (error) {
    owner.onError(redactSecrets(`Clean up settings draft (${owner.sessionId}): ${error instanceof Error ? error.message : String(error)}`));
  }
}
const KINDS: Record<string, true> = { boolean: true, string: true, secret: true, enum: true, integer: true, unsignedInteger: true, number: true, stringList: true, array: true, object: true };
function protectedValue(view: SettingValueViewData): boolean {
  return view.definition.secret || view.definition.valueType.kind === 'secret' || view.redacted
    || JSON.stringify([view.effectiveValue, view.globalValue, view.projectValue]).includes('[redacted]');
}
function rawValue(view: SettingValueViewData, scope: Scope): string {
  if (protectedValue(view)) return '';
  const value = (scope === 'global' ? view.globalValue : view.projectValue) ?? view.effectiveValue;
  return typeof value === 'string' ? value : JSON.stringify(value) ?? '';
}
function typedValue(view: SettingValueViewData, raw: string): unknown {
  const { kind, min, max, nonEmpty } = view.definition.valueType;
  if (raw.includes('[redacted]')) throw new Error('Replace redacted fields explicitly before setting this value');
  let value: unknown = raw;
  if (kind === 'boolean') {
    if (raw !== 'true' && raw !== 'false') throw new Error('Expected a boolean');
    value = raw === 'true';
  } else if (kind === 'integer' || kind === 'unsignedInteger' || kind === 'number') {
    value = Number(raw);
    if (!raw.trim() || !Number.isFinite(value) || (kind !== 'number' && !Number.isSafeInteger(value))
      || (kind === 'unsignedInteger' && Number(value) < 0)
      || (min !== null && Number(value) < min) || (max !== null && Number(value) > max)) throw new Error('Number is outside the supported range');
  } else if (kind === 'stringList' || kind === 'array' || kind === 'object') {
    try { value = JSON.parse(raw); } catch { throw new Error('Expected valid JSON'); }
    if (kind === 'object' ? value !== null && (typeof value !== 'object' || Array.isArray(value))
      : !Array.isArray(value) || (kind === 'stringList' && value.some(item => typeof item !== 'string'))) throw new Error(`Expected ${kind}`);
  } else if (KINDS[kind] !== true) throw new Error('Unsupported setting type');
  if (kind === 'enum' && !view.definition.enumValues.includes(raw)) throw new Error('Choose a supported value');
  if (nonEmpty && typeof value === 'string' && !value.trim()) throw new Error('Value must not be empty');
  return value;
}

export function SettingsPanel(props: {
  palette: Palette; client: RunnerClient; sessionId: string; onClose: () => void; onError: (message: string) => void;
}): React.JSX.Element {
  const { palette, client, sessionId } = props;
  const [catalog, setCatalog] = useState<SettingsCatalogData | null>(null);
  const [draft, setDraft] = useState<SettingsDraftData | null>(null);
  const [scope, setScope] = useState<Scope>('global');
  const [inputs, setInputs] = useState<Record<string, Input>>({});
  const [query, setQuery] = useState('');
  const [status, setStatus] = useState('');
  const [busy, setBusy] = useState(false);
  const epoch = useRef(0);
  const flight = useRef(false);
  const owner = useRef({ client, sessionId });
  const draftOwner = useRef<DraftOwner | null>(null);
  const callbacks = useRef(props);
  callbacks.current = props;
  owner.current = { client, sessionId };

  function editable(view: SettingValueViewData): boolean {
    return KINDS[view.definition.valueType.kind] === true && (scope === 'global' ? view.editableGlobal
      : view.editableProject && catalog?.projectTrusted === true)
      && !(scope === 'project' && view.definition.trustSensitive && !catalog?.projectTrusted);
  }
  async function run(context: string, action: (request: (command: RunnerCommand) => Promise<unknown>, current: () => boolean) => Promise<void>) {
    if (flight.current) return;
    flight.current = true;
    setBusy(true);
    const generation = epoch.current;
    const current = () => epoch.current === generation && owner.current.client === client && owner.current.sessionId === sessionId;
    const lease = draftOwner.current;
    const request = async (command: RunnerCommand): Promise<unknown> => {
      if (!current() || !lease || lease.retired) throw new Error('Settings owner changed');
      if (command.type === 'settings_open_draft' && lease.draftId) {
        await request({ type: 'settings_cancel', draftId: lease.draftId });
        if (!current() || lease.retired) throw new Error('Settings owner changed');
      }
      ++lease.pending;
      try {
        const response = await client.command(command, sessionId);
        if (response.success) {
          if (command.type === 'settings_cancel' && (!response.data || typeof response.data !== 'object' || !('cancelled' in response.data) || response.data.cancelled !== true)) throw new Error('Malformed settings_cancel acknowledgement');
          if (command.type === 'settings_apply' && !parseSettingsApplyOutcome(response.data)) throw new Error('Malformed settings apply outcome');
          if (command.type === 'settings_open_draft') {
            const data = response.data;
            if (data && typeof data === 'object' && 'draftId' in data && typeof data.draftId === 'string' && data.draftId.length > 0) lease.draftId = data.draftId;
          } else if ((command.type === 'settings_apply' || command.type === 'settings_cancel') && lease.draftId === command.draftId) {
            lease.draftId = null;
          }
        }
        if (!current()) throw new Error('Settings owner changed');
        if (!response.success) throw new Error(`${command.type}: ${response.error || 'Request refused'}`);
        return response.data;
      } finally {
        --lease.pending;
        void releaseDraft(lease);
      }
    };
    try { await action(request, current); }
    catch (error) {
      if (current()) {
        let message = error instanceof Error ? error.message : String(error);
        for (const view of draft?.values ?? []) {
          const raw = inputs[view.definition.key]?.raw;
          if (protectedValue(view) && raw) message = message.split(raw).join('[redacted]');
        }
        message = redactSecrets(`${context}: ${message}`);
        setStatus(message);
        callbacks.current.onError(message);
      }
    } finally {
      if (current()) { flight.current = false; setBusy(false); }
    }
  }
  function catalogSnapshot(data: unknown): SettingsCatalogData {
    const parsed = parseSettingsCatalog(data);
    if (!parsed) throw new Error('Malformed settings catalog');
    return parsed;
  }
  function draftSnapshot(data: unknown, id?: string): SettingsDraftData {
    const parsed = parseSettingsDraft(data);
    if (!parsed || parsed.scope !== scope || (id !== undefined && parsed.draftId !== id)) throw new Error('Malformed or mismatched settings draft');
    return parsed;
  }
  useEffect(() => {
    ++epoch.current;
    const lease: DraftOwner = { client, sessionId, draftId: null, pending: 0, retired: false, onError: callbacks.current.onError };
    draftOwner.current = lease;
    flight.current = false;
    setCatalog(null); setDraft(null); setInputs({}); setScope('global'); setStatus('');
    void run('Inspect settings', async request => setCatalog(catalogSnapshot(await request({ type: 'settings_inspect' }))));
    return () => { ++epoch.current; lease.retired = true; void releaseDraft(lease); };
    // Owner transitions invalidate every pending response, including refusals.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, sessionId]);

  async function stage(request: (command: RunnerCommand) => Promise<unknown>, snapshot: SettingsDraftData, key: string): Promise<SettingsDraftData> {
    const view = snapshot.values.find(row => row.definition.key === key);
    const input = inputs[key];
    if (!view || !editable(view) || !input?.dirty) throw new Error('Setting is not editable');
    const next = draftSnapshot(await request({ type: 'settings_set', draftId: snapshot.draftId, key, value: typedValue(view, input.raw) }), snapshot.draftId);
    setDraft(next);
    setInputs(previous => ({ ...previous, [key]: { raw: input.raw, dirty: false } }));
    return next;
  }
  const button = (label: string, disabled: boolean, onPress: () => void) => (
    <TouchableOpacity accessibilityLabel={label} disabled={disabled || busy} onPress={() => { if (!disabled && !flight.current) onPress(); }} style={[styles.primaryButton, { backgroundColor: palette.surface }, (disabled || busy) && styles.faded]}>
      <Text style={{ color: palette.accent }}>{label}</Text>
    </TouchableOpacity>
  );
  const cancel = (close = false) => void run('Cancel settings', async request => {
    const draftId = draftOwner.current?.draftId;
    if (draftId) await request({ type: 'settings_cancel', draftId });
    setDraft(null); setInputs({}); setStatus('Draft cancelled');
    if (close) callbacks.current.onClose();
  });
  const rows = (draft?.values ?? catalog?.values ?? []).filter(view => `${view.definition.key} ${view.definition.category} ${view.definition.description}`.toLowerCase().includes(query.toLowerCase()));
  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="Settings" onBack={() => { if (!flight.current) cancel(true); }} />
      <ScrollView contentContainerStyle={styles.form}>
        <TextInput accessibilityLabel="Search settings" value={query} onChangeText={setQuery} placeholder="Search settings" placeholderTextColor={palette.muted} style={[styles.input, { color: palette.fg, borderColor: palette.border }]} />
        {button('Refresh settings', !!draft, () => void run('Inspect settings', async request => setCatalog(catalogSnapshot(await request({ type: 'settings_inspect' })))))}
        <Text style={{ color: palette.muted }}>{`Scope: ${scope}${draft ? ' · Draft open' : ''}`}</Text>
        {catalog && <Text style={{ color: palette.muted }}>{redactSecrets(scope === 'global' ? catalog.globalPath : catalog.projectPath)}</Text>}
        {button('Global scope', !!draft, () => setScope('global'))}
        {button('Project scope', !!draft, () => setScope('project'))}
        {scope === 'project' && catalog?.projectTrusted === false && <Text style={{ color: palette.muted }}>Project is not trusted. Project settings are read-only.</Text>}
        {button('Open settings draft', !!draft || !catalog || (scope === 'project' && !catalog.projectTrusted), () => void run('Open settings draft', async request => {
          const next = draftSnapshot(await request({ type: 'settings_open_draft', scope }));
          setDraft(next); setInputs({}); setStatus('Edits are staged until Apply');
        }))}
        {draft && <>
          {button('Validate settings', false, () => void run('Validate settings', async request => {
            let next = draft;
            for (const key of Object.keys(inputs)) if (inputs[key].dirty) next = await stage(request, next, key);
            setDraft(draftSnapshot(await request({ type: 'settings_validate', draftId: next.draftId }), next.draftId));
            setStatus('Draft is valid');
          }))}
          {button('Apply settings', false, () => void run('Apply settings', async request => {
            let next = draft;
            for (const key of Object.keys(inputs)) if (inputs[key].dirty) next = await stage(request, next, key);
            const outcome = parseSettingsApplyOutcome(await request({ type: 'settings_apply', draftId: next.draftId }));
            if (!outcome) throw new Error('Malformed settings apply outcome');
            setDraft(null); setInputs({});
            setStatus(['Settings applied', outcome.appliedLive && 'Applied live', outcome.reloaded && 'Reloaded', (outcome.restartRequired || outcome.results.some(result => result.needsRestart)) && 'Restart required', (!outcome.reloaded && outcome.results.some(result => result.needsReload)) && 'Reload required'].filter(Boolean).join(' · '));
            setCatalog(catalogSnapshot(await request({ type: 'settings_inspect' })));
          }))}
          {button('Cancel settings draft', false, () => cancel())}
        </>}
        {!!status && <Text accessibilityLiveRegion="polite" style={{ color: palette.fg }}>{status}</Text>}
        {rows.map(view => {
          const { definition } = view;
          const key = definition.key;
          const secret = protectedValue(view);
          const allowed = !!draft && editable(view);
          const raw = inputs[key]?.raw ?? rawValue(view, scope);
          const change = (value: string) => { if (allowed && !flight.current) setInputs(previous => ({ ...previous, [key]: { raw: value, dirty: true } })); };
          return <View key={key} style={[styles.card, { borderColor: palette.border }]}>
            <Text style={{ color: palette.fg }}>{redactSecrets(key)}</Text>
            <Text style={{ color: palette.muted }}>{redactSecrets(`${definition.category} · ${definition.description}`)}</Text>
            <Text style={{ color: palette.muted }}>{redactSecrets(`Effective: ${secret ? 'Hidden' : JSON.stringify(view.effectiveValue)} · Source: ${view.source} · Behavior: ${definition.behavior}`)}</Text>
            {KINDS[definition.valueType.kind] !== true ? <Text style={{ color: palette.muted }}>Unsupported setting type; read-only.</Text> : draft && <>
              {definition.valueType.kind === 'boolean' && !secret ? <Switch accessibilityLabel={`Value ${key}`} value={raw === 'true'} disabled={!allowed || busy} onValueChange={value => change(String(value))} />
                : definition.valueType.kind === 'enum' && !secret ? definition.enumValues.map(value => <View key={value}>{button(`${key}: ${value}${raw === value ? ' (selected)' : ''}`, !allowed, () => change(value))}</View>)
                  : <TextInput accessibilityLabel={`Value ${key}`} value={raw} editable={allowed && !busy} secureTextEntry={secret} autoCapitalize="none" autoCorrect={false} multiline={!secret && ['object', 'array', 'stringList'].includes(definition.valueType.kind)} keyboardType={['number', 'integer', 'unsignedInteger'].includes(definition.valueType.kind) ? 'numeric' : 'default'} placeholder={secret ? 'Supply a new value explicitly' : undefined} placeholderTextColor={palette.muted} onChangeText={change} style={[styles.input, { color: palette.fg, borderColor: palette.border }]} />}
              {!allowed && <Text style={{ color: palette.muted }}>Read-only in this scope.</Text>}
              {button(`Set ${key}`, !allowed || !inputs[key]?.dirty, () => void run(`Set ${key}`, async request => { await stage(request, draft, key); setStatus('Value staged'); }))}
              {button(`Reset ${key}`, !allowed, () => void run(`Reset ${key}`, async request => {
                setDraft(draftSnapshot(await request({ type: 'settings_reset', draftId: draft.draftId, key }), draft.draftId));
                setInputs(previous => { const next = { ...previous }; delete next[key]; return next; }); setStatus('Reset staged');
              }))}
            </>}
          </View>;
        })}
        {catalog && rows.length === 0 && <Text style={{ color: palette.muted }}>No matching settings.</Text>}
      </ScrollView>
    </View>
  );
}
