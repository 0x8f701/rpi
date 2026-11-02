import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import {
  parsePersonaDetail,
  parsePersonaList,
  parsePersonaMessage,
  type PersonaDetailData,
  type PersonaRowData,
  type RunnerCommand,
} from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

const EDIT_LIMIT = 64 * 1024;
const REMOVE_NOTE = 'Removes persona.md only. memory/ and sessions/ stay under the persona root, so the persona state survives.';
const PURGE_NOTE = 'Deletes the whole persona root: persona.md, memory/, and sessions/. The persona and all its persisted state are gone.';

type Editor = { mode: 'new' | 'edit'; name: string; content: string; loading: boolean; readonly: boolean };

export function PersonasPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette, client, sessionId } = props;
  const [rows, setRows] = useState<PersonaRowData[] | null>(null);
  const [current, setCurrent] = useState<string | null>(null);
  const [detail, setDetail] = useState<PersonaDetailData | null>(null);
  const [viewName, setViewName] = useState<string | null>(null);
  const [editor, setEditor] = useState<Editor | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const [message, setMessage] = useState('');
  const [busy, setBusy] = useState(false);
  const owner = useRef({ client, sessionId, active: true, list: 0, view: 0, held: false });
  if (owner.current.client !== client || owner.current.sessionId !== sessionId) {
    owner.current.active = false;
    owner.current = { client, sessionId, active: true, list: 0, view: 0, held: false };
  }
  const errorHandler = useRef(props.onError);
  errorHandler.current = props.onError;
  const report = (error: unknown) => {
    errorHandler.current(redactSecrets(error instanceof Error ? error.message : String(error)));
  };

  const refresh = useCallback(async () => {
    const scope = owner.current;
    const generation = ++scope.list;
    try {
      const listed = await client.command({ type: 'persona_list' }, sessionId);
      if (!scope.active || generation !== scope.list) return;
      if (!listed.success) throw new Error(listed.error || 'persona_list failed');
      const payload = listed.data as { personas?: unknown } | undefined;
      if (!payload || !Array.isArray(payload.personas)) throw new Error('persona_list returned an unreadable list');
      const parsed = parsePersonaList(listed.data);
      if (parsed.length !== payload.personas.length) throw new Error('persona_list returned a malformed row');
      setRows(parsed);
      const selected = await client.command({ type: 'persona_current' }, sessionId);
      if (!scope.active || generation !== scope.list) return;
      if (!selected.success) throw new Error(selected.error || 'persona_current failed');
      const value = selected.data as { name?: unknown } | undefined;
      if (!value || !('name' in value)) throw new Error('persona_current returned an unreadable name');
      if (value.name !== null && typeof value.name !== 'string') throw new Error('persona_current returned an unreadable name');
      setCurrent(value.name);
    } catch (error) {
      if (scope.active && generation === scope.list) report(error);
    }
  }, [client, sessionId]);

  useEffect(() => {
    const scope = owner.current;
    scope.active = true;
    setRows(null);
    setCurrent(null);
    setDetail(null);
    setViewName(null);
    setEditor(null);
    setConfirm(null);
    setMessage('');
    setBusy(false);
    void refresh();
    return () => {
      scope.active = false;
      ++scope.list;
      ++scope.view;
    };
  }, [refresh]);

  const open = async (name: string, edit: boolean) => {
    const scope = owner.current;
    const generation = ++scope.view;
    setMessage('');
    setConfirm(null);
    setViewName(edit ? null : name);
    setDetail((previous) => (!edit && previous?.name === name ? previous : null));
    setEditor(edit ? { mode: 'edit', name, content: '', loading: true, readonly: false } : null);
    try {
      const response = await client.command({ type: 'persona_get', name }, sessionId);
      if (!scope.active || generation !== scope.view) return;
      if (!response.success) throw new Error(response.error || 'persona_get failed');
      const value = parsePersonaDetail(response.data);
      if (!value || value.name !== name) throw new Error('persona_get returned an unreadable definition');
      if (edit) {
        setEditor({
          mode: 'edit',
          name,
          content: value.content,
          loading: false,
          readonly: value.contentTruncated || value.content.length > EDIT_LIMIT,
        });
        return;
      }
      setDetail(value);
    } catch (error) {
      if (!scope.active || generation !== scope.view) return;
      setDetail(null);
      setMessage(error instanceof Error ? error.message : String(error));
      report(error);
    }
  };

  const mutate = async (command: RunnerCommand) => {
    const scope = owner.current;
    if (scope.held) return;
    scope.held = true;
    setBusy(true);
    try {
      const response = await client.command(command, sessionId);
      if (!scope.active) return;
      if (!response.success) throw new Error(response.error || `${command.type} failed`);
      setMessage(parsePersonaMessage(response.data) || 'Saved');
      ++scope.view;
      setEditor(null);
      setConfirm(null);
      setDetail(null);
      setViewName(null);
      await refresh();
    } catch (error) {
      if (scope.active) report(error);
    } finally {
      scope.held = false;
      if (scope.active) setBusy(false);
    }
  };

  const save = () => {
    if (!editor || editor.loading || editor.readonly) return;
    const { name, content } = editor;
    const error = !name
      ? 'persona name is required'
      : name.length > 64
        ? 'persona name must be 1..=64 characters'
        : !/^[A-Za-z0-9_-]+$/.test(name)
          ? "persona name must contain only ASCII letters, digits, '_' or '-'"
          : !content.trim()
            ? 'persona definition content is required'
            : content.length > EDIT_LIMIT
              ? 'Definition exceeds the editor limit; use /persona edit in the TUI.'
              : null;
    if (error) {
      report(error);
      return;
    }
    void mutate({ type: editor.mode === 'new' ? 'persona_create' : 'persona_edit', name, content });
  };

  const action = (label: string, icon: string, onPress: () => void, disabled = busy) => (
    <TouchableOpacity
      accessibilityRole="button"
      accessibilityLabel={label}
      disabled={disabled}
      onPress={onPress}
      style={[styles.primaryButton, { backgroundColor: palette.surface, minHeight: 44, opacity: disabled ? 0.5 : 1 }]}>
      <Icon name={icon} size={18} color={palette.accent} />
      <Text style={[styles.primaryButtonText, { color: palette.accent }]}>{label}</Text>
    </TouchableOpacity>
  );

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title="Personas"
        onBack={props.onClose}
        actions={[{ icon: 'refresh', label: 'Refresh personas', onPress: () => { void refresh(); }, disabled: busy }]}
      />
      <ScrollView contentContainerStyle={styles.form}>
        {action('New persona', 'person-add', () => {
          ++owner.current.view;
          setDetail(null);
          setViewName(null);
          setConfirm(null);
          setMessage('');
          setEditor({ mode: 'new', name: '', content: '', loading: false, readonly: false });
        })}
        {action('Clear selection', 'clear', () => { void mutate({ type: 'persona_clear' }); })}
        {!!message && <Text style={[styles.inlineError, { color: palette.muted }]}>{redactSecrets(message)}</Text>}
        {editor && (
          <View style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
            <TextInput
              accessibilityLabel="Persona name"
              value={editor.name}
              editable={!busy && editor.mode === 'new'}
              maxLength={64}
              onChangeText={(name) => setEditor({ ...editor, name })}
              placeholder="name"
              placeholderTextColor={palette.muted}
              style={[styles.input, { color: palette.fg, borderColor: palette.border }]}
            />
            <TextInput
              accessibilityLabel="Persona definition"
              value={editor.content}
              multiline
              editable={!busy && !editor.loading && !editor.readonly}
              maxLength={EDIT_LIMIT}
              onChangeText={(content) => setEditor({ ...editor, content })}
              placeholder="persona.md"
              placeholderTextColor={palette.muted}
              style={[styles.input, { color: palette.fg, borderColor: palette.border, minHeight: 160, maxHeight: 320, fontFamily: palette.mono }]}
            />
            {editor.loading && <Text style={{ color: palette.muted }}>Loading definition…</Text>}
            {editor.readonly && <Text style={{ color: palette.muted }}>Read-only definition; use /persona edit in the TUI.</Text>}
            {action(editor.mode === 'new' ? 'Create persona' : 'Save persona', 'save', save, busy || editor.loading || editor.readonly)}
            {action('Cancel editor', 'close', () => { ++owner.current.view; setEditor(null); })}
          </View>
        )}
        {viewName !== null && (
          <View style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
            <Text style={[styles.cardTitle, { color: palette.fg }]}>{redactSecrets(viewName)}</Text>
            {detail && (
              <>
                <Text selectable style={[styles.cardPre, { color: palette.fg, fontFamily: palette.mono }]}>
                  {redactSecrets(detail.content.slice(0, EDIT_LIMIT))}
                </Text>
                {(detail.contentTruncated || detail.content.length > EDIT_LIMIT) && (
                  <Text style={{ color: palette.muted }}>Definition display truncated; use /persona edit in the TUI for the full file.</Text>
                )}
              </>
            )}
            {action('Close definition', 'close', () => { ++owner.current.view; setViewName(null); setDetail(null); })}
          </View>
        )}
        {confirm !== null && (
          <View accessibilityLabel="Persona deletion confirmation" style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
            <Text style={[styles.cardTitle, { color: palette.fg }]}>{redactSecrets(confirm)}</Text>
            <Text style={{ color: palette.muted }}>{REMOVE_NOTE}</Text>
            <Text style={{ color: palette.muted }}>{PURGE_NOTE}</Text>
            {action('Remove definition (keep state)', 'delete-outline', () => { void mutate({ type: 'persona_remove', name: confirm, confirm: true }); })}
            {action('Purge root (delete everything)', 'delete-forever', () => { void mutate({ type: 'persona_purge', name: confirm, confirm: true }); })}
            {action('Cancel deletion', 'close', () => setConfirm(null))}
          </View>
        )}
        {rows === null && <Text style={[styles.emptyText, { color: palette.muted }]}>Loading personas…</Text>}
        {rows?.length === 0 && (
          <Text style={[styles.emptyText, { color: palette.muted }]}>
            No personas loaded. Create one above, or add personas/&lt;name&gt;/persona.md in the agent directory (TUI: /persona new &lt;name&gt;).
          </Text>
        )}
        {rows?.map((row) => {
          const memory = row.memoryEntries == null ? 'unknown' : `${row.memoryEntries} ${row.memoryEntries === 1 ? 'entry' : 'entries'}`;
          const sessions = row.sessionCount == null ? 'unknown' : `${row.sessionCount} ${row.sessionCount === 1 ? 'archive' : 'archives'}`;
          const source = row.source === 'user' ? 'user scope' : row.source === 'project' ? 'project scope' : row.source === 'bundled' ? 'bundled' : row.source;
          return (
            <View key={row.name} style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}>
              <Text style={[styles.cardTitle, { color: palette.fg }]}>
                {redactSecrets(row.name)}{current === row.name ? ' · Selected' : ''}
              </Text>
              <Text style={{ color: palette.muted }}>{redactSecrets(row.description)}</Text>
              <Text style={{ color: palette.muted }}>{redactSecrets(source)} · {row.trusted ? 'trusted' : 'untrusted'}</Text>
              <Text style={{ color: palette.muted }}>{redactSecrets(row.contractSummary)}</Text>
              <Text style={{ color: palette.muted }}>
                {row.stateError ? 'memory/session state unreadable' : `memory: ${memory} · sessions: ${sessions}`}
              </Text>
              {action(`View ${row.name}`, 'visibility', () => { void open(row.name, false); })}
              {action(`Edit ${row.name}`, 'edit', () => { void open(row.name, true); })}
              {action(`Select ${row.name}`, 'star', () => { void mutate({ type: 'persona_select', name: row.name }); })}
              {action(`Remove or purge ${row.name}`, 'delete', () => setConfirm(row.name))}
            </View>
          );
        })}
        <View style={styles.bottomPad} />
      </ScrollView>
    </View>
  );
}
