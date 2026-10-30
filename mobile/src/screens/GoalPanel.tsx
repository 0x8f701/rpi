// Goal panel — current goal (objective, lifecycle, token budget + usage,
// pins), lifecycle actions, and the goal journal replay view.
//
// Wire contract (crates/pi-coding/src/goal.rs, serde camelCase, snake_case
// lifecycles):
//   goal_get                                            -> GoalState
//   goal_create|pin|unpin|pause|resume|complete|drop     -> Goal
//   goal_journal                                        -> GoalEvent[]
//
// The panel refreshes from responses only — on mount, and after every settled
// mutation — and never subscribes to inbound frames: `goal_usage_charged`
// frames carry a delta plus state but no sessionId, so they cannot be
// attributed to a session (a cross-session write vector), and filtering by
// frame type alone would issue a goal_get per frame for every session without
// a goal. Mutation responses already carry the resulting goal.
//
// Mutations are sent WITHOUT `activate`: this panel only edits goal state;
// starting a work turn belongs to the slash-command path.
//
// Every async apply is scoped to the session epoch that issued it, so a
// response for a previous sessionId can never land in the new session's state.
//
// Model-derived strings are redacted at render time; source identifiers stay
// raw.

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { AppBar, styles, type Palette } from '../ui';
import { redactSecrets } from '../redact';
import type { RunnerCommand } from '../protocol';
import type { RunnerClient } from '../runnerClient';

export type GoalLifecycle = 'active' | 'paused' | 'completed' | 'dropped';

export interface GoalUsageWire {
  tokensUsed: number;
  activeTimeSeconds: number;
}

export interface GoalWire {
  id: string;
  objective: string;
  tokenBudget?: number | null;
  pins: string[];
  lifecycle: GoalLifecycle;
  pauseReason?: string | null;
  usage?: GoalUsageWire | null;
}

export interface GoalStateWire {
  current?: GoalWire | null;
  revision: number;
}

export interface GoalEventKindWire {
  type: string;
  reason?: string;
  pins?: string[];
  delta?: { tokens?: number; activeTimeSeconds?: number };
  [key: string]: unknown;
}

export interface GoalEventWire {
  revision: number;
  timestamp: string;
  kind: GoalEventKindWire | null;
  goal: GoalWire | null;
}

export const MAX_GOAL_PINS = 8;
export const MAX_GOAL_PIN_CHARS = 200;
export const MAX_JOURNAL_ROWS = 50;
export const MAX_OBJECTIVE_CHARS = 200;
export const MAX_JOURNAL_OBJECTIVE_CHARS = 120;

const GOAL_LIFECYCLES: Record<string, true> = {
  active: true,
  paused: true,
  completed: true,
  dropped: true,
};

function isGoalLifecycle(value: unknown): value is GoalLifecycle {
  return typeof value === 'string' && GOAL_LIFECYCLES[value] === true;
}

/**
 * Normalized goal. Rust omits `pins` when the vector is empty, so a missing
 * `pins` is zero pins — not a malformed goal; the identity fields (id,
 * objective, lifecycle) and the pin element type are what must hold.
 */
function parseGoal(value: unknown): GoalWire | null {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const wire = value as Record<string, unknown>;
  if (typeof wire.id !== 'string' || typeof wire.objective !== 'string' || !isGoalLifecycle(wire.lifecycle)) {
    return null;
  }
  const pins = wire.pins === undefined ? [] : wire.pins;
  if (!Array.isArray(pins) || !pins.every((pin) => typeof pin === 'string')) return null;
  const usage = wire.usage;
  const parsedUsage = usage && typeof usage === 'object' && !Array.isArray(usage)
    ? (usage as Record<string, unknown>)
    : null;
  return {
    id: wire.id,
    objective: wire.objective,
    lifecycle: wire.lifecycle,
    pins: pins as string[],
    tokenBudget: typeof wire.tokenBudget === 'number' ? wire.tokenBudget : null,
    pauseReason: typeof wire.pauseReason === 'string' ? wire.pauseReason : null,
    usage: parsedUsage
      && typeof parsedUsage.tokensUsed === 'number'
      && typeof parsedUsage.activeTimeSeconds === 'number'
      ? (parsedUsage as unknown as GoalUsageWire)
      : null,
  };
}

/** `goal_get` is the only source of a GoalState: its result is never a Goal. */
function parseGoalState(data: unknown): GoalStateWire | null {
  if (!data || typeof data !== 'object' || Array.isArray(data)) return null;
  const value = data as Record<string, unknown>;
  if (typeof value.revision !== 'number' || !Number.isFinite(value.revision)) return null;
  const current = value.current;
  if (current == null) return { current: null, revision: value.revision };
  const goal = parseGoal(current);
  if (!goal) return null;
  return { current: goal, revision: value.revision };
}

function parseGoalJournal(data: unknown): GoalEventWire[] | null {
  if (!Array.isArray(data)) return null;
  const entries: GoalEventWire[] = [];
  for (const raw of data) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) continue;
    const entry = raw as Record<string, unknown>;
    const kind = entry.kind && typeof entry.kind === 'object' && !Array.isArray(entry.kind)
      ? (entry.kind as GoalEventKindWire)
      : null;
    entries.push({
      revision: typeof entry.revision === 'number' ? entry.revision : 0,
      timestamp: typeof entry.timestamp === 'string' ? entry.timestamp : '',
      kind,
      goal: parseGoal(entry.goal),
    });
  }
  return entries;
}

/** Mobile minimum touch target. */
const TOUCH_TARGET = { minHeight: 44, minWidth: 44 };

const GOAL_MUTATIONS: Record<string, true> = {
  goal_create: true,
  goal_pin: true,
  goal_unpin: true,
  goal_pause: true,
  goal_resume: true,
  goal_complete: true,
  goal_drop: true,
};

/**
 * The goal a settled response carries: a GoalState for `goal_get`, a Goal for
 * a mutation, `undefined` for anything else or a payload that does not match —
 * an unknown payload keeps the last-good line rather than clearing it.
 */
function goalFromResponse(command: string, data: unknown): GoalWire | null | undefined {
  if (command === 'goal_get') {
    const state = parseGoalState(data);
    return state ? state.current ?? null : undefined;
  }
  if (GOAL_MUTATIONS[command] === true) return parseGoal(data) ?? undefined;
  return undefined;
}

function truncateText(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, Math.max(0, max - 1))}\u2026`;
}

function formatGoalEventKind(kind: GoalEventKindWire | null): string {
  if (!kind || typeof kind.type !== 'string') return 'updated';
  switch (kind.type) {
    case 'created':
      return 'created';
    case 'fork_cloned':
      return 'fork cloned';
    case 'paused':
      return `paused${kind.reason ? ` (${kind.reason})` : ''}`;
    case 'resumed':
      return 'resumed';
    case 'completed':
      return 'completed';
    case 'dropped':
      return 'dropped';
    case 'usage_updated':
      return `usage +${typeof kind.delta?.tokens === 'number' ? kind.delta.tokens : 0} tokens`;
    case 'pins_updated':
      return `pins updated (${Array.isArray(kind.pins) ? kind.pins.length : 0} pins)`;
    default:
      return kind.type;
  }
}

function formatGoalTime(timestamp: string): string {
  const parsed = new Date(timestamp);
  return Number.isNaN(parsed.getTime()) ? timestamp : parsed.toLocaleTimeString();
}

interface GoalAction {
  command: RunnerCommand;
  label: string;
  text: string;
  icon: string;
  danger: boolean;
}

const PAUSE: GoalAction = { command: { type: 'goal_pause' }, label: 'Pause goal', text: 'Pause', icon: 'pause', danger: false };
const RESUME: GoalAction = { command: { type: 'goal_resume' }, label: 'Resume goal', text: 'Resume', icon: 'play-arrow', danger: false };
const COMPLETE: GoalAction = { command: { type: 'goal_complete' }, label: 'Complete goal', text: 'Complete', icon: 'check', danger: false };
const DROP: GoalAction = { command: { type: 'goal_drop' }, label: 'Drop goal', text: 'Drop', icon: 'delete', danger: true };

/** Lifecycle gate: a settled goal offers no actions. */
const GOAL_ACTIONS: Record<GoalLifecycle, GoalAction[]> = {
  active: [PAUSE, COMPLETE, DROP],
  paused: [RESUME, COMPLETE, DROP],
  completed: [],
  dropped: [],
};

export function GoalPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette, sessionId } = props;
  const rawSessionId = sessionId.trim();

  const [state, setState] = useState<GoalStateWire | null>(null);
  const [journal, setJournal] = useState<GoalEventWire[]>([]);
  const [objective, setObjective] = useState('');
  const [pinText, setPinText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  // Latest-value refs: async work started before a re-render must still reach
  // the current client/session and report through the current onError.
  const clientRef = useRef(props.client);
  const sessionIdRef = useRef(sessionId);
  const onErrorRef = useRef(props.onError);
  clientRef.current = props.client;
  sessionIdRef.current = sessionId;
  onErrorRef.current = props.onError;

  // Epoch of the session whose responses may still be applied; one in-flight
  // fetch per operation per epoch.
  const sessionEpochRef = useRef(0);
  const goalInFlightRef = useRef<number | null>(null);
  const journalInFlightRef = useRef<number | null>(null);
  const busyRef = useRef(false);

  const goal = state?.current ?? null;
  const lifecycle: GoalLifecycle | null = goal ? goal.lifecycle : null;
  const pins = goal?.pins ?? [];
  const usage = goal?.usage ?? null;

  /** `null` = loaded without a goal, `undefined` = no knowledge, else adopt. */
  const apply = useCallback((epoch: number, next: GoalWire | null | undefined) => {
    if (epoch !== sessionEpochRef.current) return;
    setState((prev) => {
      const revision = prev?.revision ?? 0;
      if (next === undefined) return { current: prev?.current ?? null, revision };
      return { current: next, revision };
    });
  }, []);

  const refreshGoal = useCallback(async (epoch: number) => {
    if (goalInFlightRef.current === epoch) return;
    goalInFlightRef.current = epoch;
    try {
      const response = await clientRef.current.command({ type: 'goal_get' }, sessionIdRef.current);
      if (epoch !== sessionEpochRef.current) return;
      if (!response.success) {
        const message = response.error ?? 'goal_get failed';
        setError(message);
        onErrorRef.current(message);
        return;
      }
      const parsed = parseGoalState(response.data);
      if (!parsed) {
        const message = 'malformed goal_get response';
        setError(message);
        onErrorRef.current(message);
        return;
      }
      apply(epoch, parsed.current ?? null);
    } catch (err) {
      if (epoch !== sessionEpochRef.current) return;
      const message = err instanceof Error ? err.message : String(err);
      setError(message);
      onErrorRef.current(message);
    } finally {
      if (goalInFlightRef.current === epoch) goalInFlightRef.current = null;
    }
  }, [apply]);

  const refreshJournal = useCallback(async (epoch: number) => {
    if (journalInFlightRef.current === epoch) return;
    journalInFlightRef.current = epoch;
    try {
      const response = await clientRef.current.command({ type: 'goal_journal' }, sessionIdRef.current);
      if (epoch !== sessionEpochRef.current) return;
      if (!response.success) {
        const message = response.error ?? 'goal_journal failed';
        setError(message);
        onErrorRef.current(message);
        return;
      }
      const entries = parseGoalJournal(response.data);
      if (!entries) {
        const message = 'malformed goal_journal response';
        setError(message);
        onErrorRef.current(message);
        return;
      }
      setJournal(entries.length > MAX_JOURNAL_ROWS ? entries.slice(entries.length - MAX_JOURNAL_ROWS) : entries);
    } catch (err) {
      if (epoch !== sessionEpochRef.current) return;
      const message = err instanceof Error ? err.message : String(err);
      setError(message);
      onErrorRef.current(message);
    } finally {
      if (journalInFlightRef.current === epoch) journalInFlightRef.current = null;
    }
  }, []);

  useEffect(() => {
    const epoch = sessionEpochRef.current + 1;
    sessionEpochRef.current = epoch;
    goalInFlightRef.current = null;
    journalInFlightRef.current = null;
    busyRef.current = false;
    setBusy(false);
    setError('');
    setState(null);
    setJournal([]);
    setObjective('');
    setPinText('');
    void refreshGoal(epoch);
    void refreshJournal(epoch);
    const off = clientRef.current.subscribeFrames((frame) => {
      if (!frame || typeof frame !== 'object') return;
      const record = frame as Record<string, unknown>;
      if (record.sessionId !== sessionIdRef.current) return;
      if (record.type !== 'goal_updated' && record.type !== 'goal_usage_charged') return;
      void refreshGoal(sessionEpochRef.current);
      void refreshJournal(sessionEpochRef.current);
    });
    return () => {
      off();
    };
  }, [rawSessionId, props.client, refreshGoal, refreshJournal]);

  /** One mutation at a time; a refusal leaves every draft untouched. */
  const run = useCallback(async (command: RunnerCommand): Promise<boolean> => {
    if (busyRef.current) return false;
    busyRef.current = true;
    setBusy(true);
    setError('');
    const epoch = sessionEpochRef.current;
    try {
      const response = await clientRef.current.command(command, sessionIdRef.current);
      if (epoch !== sessionEpochRef.current) return false;
      if (!response.success) {
        const message = response.error ?? `${command.type} failed`;
        setError(message);
        onErrorRef.current(message);
        return false;
      }
      const next = goalFromResponse(command.type, response.data);
      if (next === undefined) {
        const message = `malformed ${command.type} response`;
        setError(message);
        onErrorRef.current(message);
        return false;
      }
      apply(epoch, next);
      await refreshJournal(epoch);
      return true;
    } catch (err) {
      if (epoch !== sessionEpochRef.current) return false;
      const message = err instanceof Error ? err.message : String(err);
      setError(message);
      onErrorRef.current(message);
      return false;
    } finally {
      busyRef.current = false;
      if (epoch === sessionEpochRef.current) setBusy(false);
    }
  }, [apply, refreshJournal]);

  const onCreate = () => {
    const text = objective.trim();
    if (text === '' || busyRef.current) return;
    void run({ type: 'goal_create', objective: text }).then((done) => {
      if (done) setObjective('');
    });
  };

  const onPin = () => {
    const text = pinText.trim();
    if (text === '' || busyRef.current || pins.length >= MAX_GOAL_PINS) return;
    void run({ type: 'goal_pin', text }).then((done) => {
      if (done) setPinText('');
    });
  };

  const onUnpin = (index: number) => {
    if (busyRef.current) return;
    void run({ type: 'goal_unpin', index });
  };

  const budgetLine = goal
    ? goal.tokenBudget != null
      ? `${goal.tokenBudget} token budget`
      : 'no token budget'
    : '';
  const usageLine = goal && usage
    ? `${usage.tokensUsed}/${goal.tokenBudget ?? '\u221E'} tokens \u00B7 ${usage.activeTimeSeconds}s active`
    : '';
  const percent = goal && usage && goal.tokenBudget
    ? Math.min(100, Math.round((usage.tokensUsed / goal.tokenBudget) * 100))
    : null;
  const actions = lifecycle ? GOAL_ACTIONS[lifecycle] : [];

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="Goal" onBack={props.onClose} />
      {error !== '' && (
        <Text style={[styles.inlineError, { color: palette.error }]}>{redactSecrets(error)}</Text>
      )}
      <ScrollView contentContainerStyle={styles.form}>
        {goal ? (
          <>
            <Text style={[styles.formSection, { color: palette.muted }]} accessibilityLabel="Goal status">
              {goal.pauseReason ? `${goal.lifecycle} (${goal.pauseReason})` : goal.lifecycle}
            </Text>
            <Text style={[styles.rowTitle, { color: palette.fg }]} accessibilityLabel="Goal objective">
              {truncateText(redactSecrets(goal.objective), MAX_OBJECTIVE_CHARS)}
            </Text>
            <Text style={[styles.rowSub, { color: palette.muted }]} accessibilityLabel="Goal budget">
              {budgetLine}
            </Text>
            {usageLine !== '' && (
              <Text style={[styles.rowSub, { color: palette.muted }]} accessibilityLabel="Goal usage">
                {percent == null ? usageLine : `${usageLine} \u00B7 ${percent}%`}
              </Text>
            )}

            {actions.length > 0 && (
              <View style={{ flexDirection: 'row', flexWrap: 'wrap', gap: 8 }}>
                {actions.map((action) => (
                  <TouchableOpacity
                    key={action.command.type}
                    accessibilityLabel={action.label}
                    disabled={busy}
                    style={[
                      styles.primaryButton,
                      TOUCH_TARGET,
                      { backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border, flexGrow: 1 },
                      busy && styles.faded,
                    ]}
                    onPress={() => void run(action.command)}>
                    <Icon name={action.icon} size={18} color={action.danger ? palette.error : palette.fg} />
                    <Text style={[styles.primaryButtonText, { color: action.danger ? palette.error : palette.fg }]}>
                      {action.text}
                    </Text>
                  </TouchableOpacity>
                ))}
              </View>
            )}

            <Text style={[styles.formSection, { color: palette.muted }]}>
              {`Pins (${pins.length}/${MAX_GOAL_PINS})`}
            </Text>
            <View style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
              <TextInput
                accessibilityLabel="Goal pin"
                style={[styles.input, TOUCH_TARGET, { flex: 1, backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
                value={pinText}
                onChangeText={setPinText}
                placeholder={`Pin text (max ${MAX_GOAL_PIN_CHARS} chars)`}
                placeholderTextColor={palette.muted}
                maxLength={MAX_GOAL_PIN_CHARS}
                autoCapitalize="none"
                autoCorrect={false}
              />
              <TouchableOpacity
                accessibilityLabel="Add pin"
                disabled={busy || pinText.trim() === '' || pins.length >= MAX_GOAL_PINS}
                style={[
                  styles.primaryButton,
                  TOUCH_TARGET,
                  { backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border },
                  (busy || pinText.trim() === '' || pins.length >= MAX_GOAL_PINS) && styles.faded,
                ]}
                onPress={onPin}>
                <Icon name="push-pin" size={18} color={palette.fg} />
                <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Pin</Text>
              </TouchableOpacity>
            </View>
            {pins.length === 0 ? (
              <Text style={[styles.emptyText, { color: palette.muted }]}>No pins.</Text>
            ) : (
              pins.map((pin, index) => (
                <View
                  key={`${index}-${pin}`}
                  style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
                  <Text style={[styles.rowSub, { color: palette.fg, flex: 1 }]} numberOfLines={2}>
                    {redactSecrets(pin)}
                  </Text>
                  <TouchableOpacity
                    accessibilityLabel={`Remove pin ${index + 1}`}
                    disabled={busy}
                    style={[TOUCH_TARGET, { alignItems: 'center', justifyContent: 'center' }, busy && styles.faded]}
                    onPress={() => onUnpin(index)}>
                    <Icon name="close" size={18} color={palette.muted} />
                  </TouchableOpacity>
                </View>
              ))
            )}
          </>
        ) : state === null ? (
          <Text style={[styles.emptyText, { color: palette.muted }]}>--</Text>
        ) : (
          <>
            <Text style={[styles.emptyText, { color: palette.muted }]}>No goal. Create one.</Text>
            <TextInput
              accessibilityLabel="Goal objective input"
              style={[styles.input, TOUCH_TARGET, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
              value={objective}
              onChangeText={setObjective}
              placeholder="Objective — why future turns may continue"
              placeholderTextColor={palette.muted}
              multiline
              autoCapitalize="none"
              autoCorrect={false}
            />
            <TouchableOpacity
              accessibilityLabel="Create goal"
              style={[
                styles.primaryButton,
                TOUCH_TARGET,
                { backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border },
                (busy || objective.trim() === '') && styles.faded,
              ]}
              disabled={busy || objective.trim() === ''}
              onPress={onCreate}>
              <Icon name="add" size={18} color={palette.fg} />
              <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Create goal</Text>
            </TouchableOpacity>
          </>
        )}

        <Text style={[styles.formSection, { color: palette.muted }]}>Journal</Text>
        {journal.length === 0 ? (
          <Text style={[styles.emptyText, { color: palette.muted }]}>No goal events yet.</Text>
        ) : (
          journal.map((entry, index) => (
            <View key={`${entry.revision}-${index}`} style={{ gap: 2 }}>
              <Text style={[styles.rowSub, { color: palette.muted }]} accessibilityLabel={`Journal ${index + 1}`}>
                {`${formatGoalTime(entry.timestamp)} \u00B7 ${formatGoalEventKind(entry.kind)}`}
              </Text>
              {entry.goal && entry.goal.objective !== '' && (
                <Text style={[styles.rowSub, { color: palette.fg }]} numberOfLines={1}>
                  {truncateText(redactSecrets(entry.goal.objective), MAX_JOURNAL_OBJECTIVE_CHARS)}
                </Text>
              )}
            </View>
          ))
        )}
      </ScrollView>
    </View>
  );
}
