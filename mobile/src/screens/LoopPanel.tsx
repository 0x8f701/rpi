import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import type { RunnerCommand } from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

const INTERVAL_CHOICES = ['30s', '1m', '5m', '15m', '1h', '4h'] as const;
const INTERVAL_CHOICE: Record<string, true> = {
  '30s': true, '1m': true, '5m': true, '15m': true, '1h': true, '4h': true,
};
const PROMPT_DISPLAY_CHARS = 80;

export type LoopLifecycle = 'scheduled' | 'queued' | 'running' | 'skipped' | 'failed';

interface LoopWire {
  id: string;
  intervalSecs: number;
  prompt: string;
  durable: boolean;
  createdAt: string;
  lastFiredAt: string | null;
  expiresAt: string;
  runCount: number;
}

interface LoopRow extends LoopWire {
  lifecycle: LoopLifecycle;
}

type PendingAction = { kind: 'cancel' | 'delete'; taskId: string } | null;

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function compactInterval(seconds: number): string {
  if (seconds % 86_400 === 0) return `${seconds / 86_400}d`;
  if (seconds % 3_600 === 0) return `${seconds / 3_600}h`;
  if (seconds % 60 === 0) return `${seconds / 60}m`;
  return `${seconds}s`;
}

function humanSchedule(seconds: number): string {
  const [count, unit] = seconds % 86_400 === 0 ? [seconds / 86_400, 'day']
    : seconds % 3_600 === 0 ? [seconds / 3_600, 'hour']
      : seconds % 60 === 0 ? [seconds / 60, 'minute']
        : [seconds, 'second'];
  return count === 1 ? `every 1 ${unit}` : `every ${count} ${unit}s`;
}
function parseLoopTask(value: unknown): LoopWire {
  const row = record(value);
  const intervalSecs = row && typeof row.intervalSecs === 'number' ? row.intervalSecs : NaN;
  const runCount = row && typeof row.runCount === 'number' ? row.runCount : NaN;
  const lastFiredAt = row && (row.lastFiredAt === undefined || row.lastFiredAt === null)
    ? null
    : row && typeof row.lastFiredAt === 'string' ? row.lastFiredAt : undefined;
  if (!row || typeof row.id !== 'string' || row.id === ''
    || !Number.isInteger(intervalSecs) || intervalSecs <= 0
    || typeof row.prompt !== 'string'
    || typeof row.durable !== 'boolean'
    || typeof row.createdAt !== 'string' || row.createdAt === ''
    || lastFiredAt === undefined
    || typeof row.expiresAt !== 'string' || row.expiresAt === ''
    || !Number.isInteger(runCount) || runCount < 0) {
    throw new Error('Invalid loop task');
  }
  return {
    id: row.id,
    intervalSecs,
    prompt: row.prompt,
    durable: row.durable,
    createdAt: row.createdAt,
    lastFiredAt,
    expiresAt: row.expiresAt,
    runCount,
  };
}

function parseLoopList(value: unknown): LoopWire[] {
  if (!Array.isArray(value)) throw new Error('Invalid loop list');
  const ids = new Set<string>();
  return value.map((item) => {
    const task = parseLoopTask(item);
    if (ids.has(task.id)) throw new Error('Invalid loop list');
    ids.add(task.id);
    return task;
  });
}
function eventTaskId(event: Record<string, unknown>): string | null {
  if (typeof event.taskId === 'string' && event.taskId !== '') return event.taskId;
  if (typeof event.task_id === 'string' && event.task_id !== '') return event.task_id;
  return null;
}

function adoptList(next: LoopWire[], previous: LoopRow[]): LoopRow[] {
  const life = new Map(previous.map((row) => [row.id, row.lifecycle]));
  return next.map((task) => ({ ...task, lifecycle: life.get(task.id) ?? 'scheduled' }));
}

function upsertLoop(rows: LoopRow[], task: LoopWire, lifecycle?: LoopLifecycle): LoopRow[] {
  const next: LoopRow = {
    ...task,
    lifecycle: lifecycle ?? rows.find((row) => row.id === task.id)?.lifecycle ?? 'scheduled',
  };
  const index = rows.findIndex((row) => row.id === task.id);
  if (index < 0) return [...rows, next];
  return rows.map((row, i) => (i === index ? next : row));
}

function intervalButtons(
  palette: Palette,
  value: string,
  disabled: boolean,
  prefix: string,
  onPick: (interval: string) => void,
): React.JSX.Element {
  const choices = INTERVAL_CHOICE[value]
    ? INTERVAL_CHOICES
    : [value, ...INTERVAL_CHOICES];
  return (
    <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
      {choices.map((choice) => {
        const selected = choice === value;
        return (
          <TouchableOpacity
            key={choice}
            disabled={disabled}
            accessibilityLabel={`${prefix} ${choice}`}
            style={[
              styles.primaryButton,
              {
                marginTop: 0,
                minHeight: 44,
                minWidth: 44,
                paddingVertical: 12,
                paddingHorizontal: 12,
                backgroundColor: selected ? palette.accent : palette.surface,
                borderWidth: 1,
                borderColor: palette.border,
              },
              disabled && styles.faded,
            ]}
            onPress={() => onPick(choice)}>
            <Text style={[styles.primaryButtonText, { color: selected ? palette.bg : palette.fg }]}>{choice}</Text>
          </TouchableOpacity>
        );
      })}
    </View>
  );
}

export function LoopPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette, client, sessionId } = props;
  const [loops, setLoops] = useState<LoopRow[]>([]);
  const [createInterval, setCreateInterval] = useState<string>('5m');
  const [createPrompt, setCreatePrompt] = useState('');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [updateInterval, setUpdateInterval] = useState('5m');
  const [updatePrompt, setUpdatePrompt] = useState('');
  const [pending, setPending] = useState<PendingAction>(null);
  const [busy, setBusy] = useState(false);
  const owner = useRef({ client, sessionId, active: true, operation: false, refresh: 0, listing: false, wanted: false });
  if (owner.current.client !== client || owner.current.sessionId !== sessionId) {
    owner.current.active = false;
    owner.current = { client, sessionId, active: true, operation: false, refresh: 0, listing: false, wanted: false };
  }
  const errorHandler = useRef(props.onError);
  errorHandler.current = props.onError;
  const report = (context: string, error: unknown) => {
    errorHandler.current(redactSecrets(`${context}: ${error instanceof Error ? error.message : String(error)}`));
  };

  const refresh = useCallback(async () => {
    const current = owner.current;
    if (current.listing) {
      current.wanted = true;
      return;
    }
    current.listing = true;
    const request = ++current.refresh;
    try {
      const response = await client.command({ type: 'loop_list' }, sessionId);
      if (!current.active || current !== owner.current || request !== current.refresh) return;
      if (!response.success) throw new Error(response.error || 'Request refused');
      const next = parseLoopList(response.data);
      setLoops((previous) => adoptList(next, previous));
      setSelectedId((id) => next.some((task) => task.id === id) ? id : null);
      setPending((action) => action && next.some((task) => task.id === action.taskId) ? action : null);
    } catch (error) {
      if (current.active && current === owner.current && request === current.refresh) report('Refresh loops', error);
    } finally {
      if (current === owner.current) {
        current.listing = false;
        if (current.wanted && current.active) {
          current.wanted = false;
          void refresh();
        }
      }
    }
  }, [client, sessionId]);

  useEffect(() => {
    const current = owner.current;
    current.active = true;
    setLoops([]);
    setCreateInterval('5m');
    setCreatePrompt('');
    setSelectedId(null);
    setUpdateInterval('5m');
    setUpdatePrompt('');
    setPending(null);
    setBusy(false);
    void refresh();
    const off = client.subscribeFrames((frame) => {
      const event = record(frame);
      if (!event || typeof event.type !== 'string') return;
      if (event.sessionId !== sessionId) return;
      const apply = (fn: (rows: LoopRow[]) => LoopRow[]) => {
        if (!current.active || current !== owner.current) return;
        setLoops(fn);
      };
      switch (event.type) {
        case 'loop_created':
        case 'loop_updated': {
          try {
            const task = parseLoopTask(event.task);
            apply((rows) => upsertLoop(rows, task, event.type === 'loop_created' ? 'scheduled' : undefined));
          } catch (error) {
            report('Loop event', error);
          }
          return;
        }
        case 'loop_queued':
        case 'loop_fired':
        case 'loop_skipped':
        case 'loop_finished':
        case 'loop_failed': {
          const id = eventTaskId(event);
          if (!id) return;
          const lifecycle: LoopLifecycle = event.type === 'loop_queued' ? 'queued'
            : event.type === 'loop_fired' ? 'running'
              : event.type === 'loop_skipped' ? 'skipped'
                : event.type === 'loop_failed' ? 'failed'
                  : 'scheduled';
          apply((rows) => rows.map((row) => row.id === id ? { ...row, lifecycle } : row));
          if (event.type === 'loop_failed' && typeof event.message === 'string' && event.message !== '') {
            report(`Loop ${id}`, event.message);
          }
          return;
        }
        case 'loop_removed': {
          const id = eventTaskId(event);
          if (!id) return;
          apply((rows) => rows.filter((row) => row.id !== id));
          setSelectedId((currentId) => currentId === id ? null : currentId);
          setPending((action) => action?.taskId === id ? null : action);
          return;
        }
        case 'loop_scheduler_failed': {
          if (typeof event.message === 'string' && event.message !== '') report('Loop scheduler', event.message);
          return;
        }
        default:
          return;
      }
    });
    return () => { current.active = false; off(); };
  }, [client, sessionId, refresh]);

  const mutate = async (label: string, command: RunnerCommand, onSuccess: (data: unknown) => void) => {
    const current = owner.current;
    if (!current.active || current.operation) return;
    current.operation = true;
    ++current.refresh;
    setBusy(true);
    try {
      const response = await client.command(command, sessionId);
      if (!current.active || current !== owner.current) return;
      if (!response.success) throw new Error(response.error || 'Request refused');
      onSuccess(response.data);
      await refresh();
    } catch (error) {
      if (current.active && current === owner.current) report(label, error);
    } finally {
      current.operation = false;
      if (current.active && current === owner.current) setBusy(false);
    }
  };

  const createLoop = () => {
    const prompt = createPrompt.trim();
    if (!prompt || busy) return;
    void mutate('Create loop', {
      type: 'loop_create',
      interval: createInterval,
      prompt,
      fireImmediately: true,
      durable: false,
    }, (data) => {
      const task = parseLoopTask(data);
      setLoops((rows) => upsertLoop(rows, task, 'scheduled'));
      setCreatePrompt((draft) => draft.trim() === prompt ? '' : draft);
    });
  };

  const updateLoop = () => {
    if (!selectedId || busy) return;
    const selected = loops.find((row) => row.id === selectedId);
    if (!selected) return;
    const interval = updateInterval.trim();
    const prompt = updatePrompt.trim();
    const intervalChanged = interval !== '' && interval !== compactInterval(selected.intervalSecs);
    const promptChanged = prompt !== selected.prompt;
    if (!intervalChanged && !promptChanged) return;
    const command: Extract<RunnerCommand, { type: 'loop_update' }> = { type: 'loop_update', taskId: selectedId };
    if (intervalChanged) command.interval = interval;
    if (promptChanged) command.prompt = prompt;
    void mutate('Update loop', command, (data) => {
      const task = parseLoopTask(data);
      setLoops((rows) => upsertLoop(rows, task));
    });
  };

  const requestAction = (kind: 'cancel' | 'delete', taskId: string) => {
    if (busy) return;
    if (pending?.kind !== kind || pending.taskId !== taskId) {
      setPending({ kind, taskId });
      return;
    }
    setPending(null);
    const command: RunnerCommand = kind === 'cancel'
      ? { type: 'loop_cancel', taskId }
      : { type: 'loop_delete', taskId };
    void mutate(kind === 'cancel' ? 'Cancel loop' : 'Delete loop', command, (data) => {
      if (data !== true) throw new Error(`no active loop with id ${taskId}`);
      setLoops((rows) => rows.filter((row) => row.id !== taskId));
      setSelectedId((id) => id === taskId ? null : id);
    });
  };

  const selectLoop = (row: LoopRow) => {
    setSelectedId(row.id);
    setUpdateInterval(compactInterval(row.intervalSecs));
    setUpdatePrompt(row.prompt);
    setPending(null);
  };

  const selected = loops.find((row) => row.id === selectedId);

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title="Loop"
        onBack={props.onClose}
        actions={[{ icon: 'refresh', label: 'Refresh loops', onPress: () => void refresh(), disabled: busy }]}
      />
      <ScrollView contentContainerStyle={styles.form}>
        <Text style={[styles.formSection, { color: palette.muted }]}>New loop</Text>
        {intervalButtons(palette, createInterval, busy, 'Create interval', setCreateInterval)}
        <TextInput
          style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg, minHeight: 44 }]}
          value={createPrompt}
          onChangeText={setCreatePrompt}
          placeholder="Prompt to run on each fire"
          accessibilityLabel="Loop prompt"
          placeholderTextColor={palette.muted}
          editable={!busy}
          multiline
        />
        <TouchableOpacity
          accessibilityLabel="Create loop"
          style={[styles.primaryButton, { backgroundColor: palette.accent, minHeight: 44 }, (!createPrompt.trim() || busy) && styles.faded]}
          disabled={!createPrompt.trim() || busy}
          onPress={createLoop}>
          <Icon name="repeat" size={22} color={palette.bg} />
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{busy ? 'Working…' : 'Create loop'}</Text>
        </TouchableOpacity>

        <Text style={[styles.formSection, { color: palette.muted }]}>{`Active loops (${loops.length})`}</Text>
        {loops.length === 0 && (
          <Text style={[styles.emptyText, { color: palette.muted }]}>No active loops.</Text>
        )}
        {loops.map((row) => {
          const cancelPending = pending?.kind === 'cancel' && pending.taskId === row.id;
          const deletePending = pending?.kind === 'delete' && pending.taskId === row.id;
          const redacted = redactSecrets(row.prompt);
          const shown = redacted.length <= PROMPT_DISPLAY_CHARS ? redacted : `${redacted.slice(0, PROMPT_DISPLAY_CHARS)}…`;
          return (
            <View key={row.id} style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}>
              <TouchableOpacity
                style={styles.rowBody}
                accessibilityLabel={`Select loop ${row.id}`}
                onPress={() => selectLoop(row)}>
                <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={2}>{shown || row.id}</Text>
                <Text style={[styles.rowSub, { color: palette.muted }]}>
                  {`${compactInterval(row.intervalSecs)} · ${humanSchedule(row.intervalSecs)} · ${row.lifecycle}${row.durable ? ' · durable' : ''}`}
                </Text>
              </TouchableOpacity>
              <TouchableOpacity
                disabled={busy}
                hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
                style={{ minHeight: 44, minWidth: 44, alignItems: 'center', justifyContent: 'center' }}
                accessibilityLabel={cancelPending ? `Confirm cancel current run ${row.id}` : `Cancel current run ${row.id}`}
                onPress={() => requestAction('cancel', row.id)}>
                {cancelPending
                  ? <Text style={{ color: palette.error }}>Cancel run?</Text>
                  : <Icon name="stop" size={22} color={palette.error} />}
              </TouchableOpacity>
              <TouchableOpacity
                disabled={busy}
                hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}
                style={{ minHeight: 44, minWidth: 44, alignItems: 'center', justifyContent: 'center' }}
                accessibilityLabel={deletePending ? `Confirm delete schedule ${row.id}` : `Delete schedule ${row.id}`}
                onPress={() => requestAction('delete', row.id)}>
                {deletePending
                  ? <Text style={{ color: palette.error }}>Delete schedule?</Text>
                  : <Icon name="delete-outline" size={22} color={palette.error} />}
              </TouchableOpacity>
            </View>
          );
        })}

        {selected && (
          <View style={{ gap: 14 }}>
            <Text style={[styles.formSection, { color: palette.muted }]}>{`Update ${selected.id}`}</Text>
            <TextInput
              style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg, minHeight: 44 }]}
              value={updateInterval}
              onChangeText={setUpdateInterval}
              placeholder="Interval"
              accessibilityLabel="Update interval"
              placeholderTextColor={palette.muted}
              editable={!busy}
            />
            {intervalButtons(palette, updateInterval, busy, 'Update interval', setUpdateInterval)}
            <TextInput
              style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg, minHeight: 44 }]}
              value={updatePrompt}
              onChangeText={setUpdatePrompt}
              placeholder="Updated prompt"
              accessibilityLabel="Update prompt"
              placeholderTextColor={palette.muted}
              editable={!busy}
              multiline
            />
            <TouchableOpacity
              accessibilityLabel="Update loop"
              style={[styles.primaryButton, { backgroundColor: palette.accent, minHeight: 44 }, busy && styles.faded]}
              disabled={busy}
              onPress={updateLoop}>
              <Icon name="edit" size={22} color={palette.bg} />
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Update loop</Text>
            </TouchableOpacity>
          </View>
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
    </View>
  );
}
