import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TouchableOpacity, View } from 'react-native';
import { parseAvailableModels, parseThinkingLevels } from '../codeReviewSnapshot';

type SteeringMode = 'all' | 'one-at-a-time';

/** `null` = no mode the panel may act on has been reported yet. */
function parseSteeringMode(data: unknown): SteeringMode | null {
  if (!data || typeof data !== 'object') return null;
  const mode = (data as { steeringMode?: unknown }).steeringMode;
  return mode === 'all' || mode === 'one-at-a-time' ? mode : null;
}
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

export function SessionSettingsPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette } = props;
  const [models, setModels] = useState<Array<{ id: string; name: string; provider: string }> | null>(null);
  const [levels, setLevels] = useState<string[] | null>(null);
  /** Unknown until get_state reports a mode this panel may act on: the toggle
   *  renders `--` and stays disabled rather than guessing the default. */
  const [queueMode, setQueueMode] = useState<SteeringMode | null>(null);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  /** Only the newest load owns the panel: a response from a superseded
   *  client/session must never publish its mode or re-arm the toggle. */
  const loadGenRef = useRef(0);

  const load = useCallback(async () => {
    const gen = loadGenRef.current + 1;
    loadGenRef.current = gen;
    setQueueMode(null);
    try {
      const modelResponse = await props.client.command({ type: 'get_available_models' }, props.sessionId);
      if (loadGenRef.current !== gen) return;
      if (!modelResponse.success) {
        setModels(null);
        props.onError(modelResponse.error || 'Models failed');
      } else {
        setModels(parseAvailableModels(modelResponse.data));
      }
      const levelResponse = await props.client.command({ type: 'get_available_thinking_levels' }, props.sessionId);
      if (loadGenRef.current !== gen) return;
      if (!levelResponse.success) {
        setLevels(null);
        props.onError(levelResponse.error || 'Thinking levels failed');
      } else {
        setLevels(parseThinkingLevels(levelResponse.data));
      }
      try {
        const stateResponse = await props.client.command({ type: 'get_state' }, props.sessionId);
        if (loadGenRef.current !== gen) return;
        if (!stateResponse.success) {
          props.onError(`get_state failed${stateResponse.error ? `: ${stateResponse.error}` : ''}`);
          return;
        }
        const mode = parseSteeringMode(stateResponse.data);
        if (mode === null) {
          props.onError('get_state failed: malformed steering mode');
          return;
        }
        setQueueMode(mode);
      } catch (err) {
        if (loadGenRef.current !== gen) return;
        props.onError(`get_state failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    } catch (err) {
      if (loadGenRef.current !== gen) return;
      props.onError(err instanceof Error ? err.message : String(err));
    }
  }, [props.client, props.onError, props.sessionId]);

  useEffect(() => {
    void load();
    return () => {
      // A retired panel owns nothing: bump the generation so the in-flight load
      // stops before its next request and never publishes state or a banner for
      // a surface that no longer exists.
      loadGenRef.current += 1;
    };
  }, [load]);

  const run = async (fn: () => Promise<void>) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      await fn();
    } catch (err) {
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="Settings" onBack={props.onClose} />
      <ScrollView contentContainerStyle={styles.form}>
        <Text style={[styles.formSection, { color: palette.muted }]}>Model</Text>
        {models === null && <Text style={[styles.rowTitle, { color: palette.muted }]}>--</Text>}
        {models && models.length === 0 && <Text style={[styles.emptyText, { color: palette.muted }]}>No models.</Text>}
        {models?.map((model) => {
          const spec = `${model.provider}/${model.id}`;
          return (
            <TouchableOpacity
              key={spec}
              style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
              disabled={busy}
              onPress={() => void run(async () => {
                const response = await props.client.command({ type: 'set_model', provider: model.provider, modelId: model.id }, props.sessionId);
                if (!response.success) props.onError(response.error || 'Model change failed');
              })}
              accessibilityLabel={`Set model ${spec}`}>
              <View style={styles.rowBody}>
                <Text style={[styles.rowTitle, { color: palette.fg }]}>{model.name || model.id}</Text>
                <Text style={[styles.rowSub, { color: palette.muted }]}>{spec}</Text>
              </View>
            </TouchableOpacity>
          );
        })}
        <Text style={[styles.formSection, { color: palette.muted }]}>Thinking</Text>
        {levels === null && <Text style={[styles.rowTitle, { color: palette.muted }]}>--</Text>}
        {levels && levels.length === 0 && <Text style={[styles.emptyText, { color: palette.muted }]}>No levels.</Text>}
        {levels?.map((level) => (
          <TouchableOpacity
            key={level}
            style={[styles.primaryButton, { backgroundColor: palette.surface, borderWidth: 1, borderColor: palette.border }]}
            disabled={busy}
            onPress={() => void run(async () => {
              const response = await props.client.command({ type: 'set_thinking_level', level }, props.sessionId);
              if (!response.success) props.onError(response.error || 'Thinking change failed');
            })}
            accessibilityLabel={`Set thinking ${level}`}>
            <Text style={[styles.primaryButtonText, { color: palette.fg }]}>{level}</Text>
          </TouchableOpacity>
        ))}
        <Text style={[styles.formSection, { color: palette.muted }]}>Queue</Text>
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent }]}
          disabled={busy || queueMode === null}
          onPress={() => {
            if (busyRef.current || queueMode === null) return;
            const current = queueMode;
            void run(async () => {
              const next: SteeringMode = current === 'all' ? 'one-at-a-time' : 'all';
              const response = await props.client.command({ type: 'set_steering_mode', mode: next }, props.sessionId);
              if (!response.success) {
                props.onError(response.error || 'Queue mode failed');
                return;
              }
              setQueueMode(next);
            });
          }}
          accessibilityLabel="Queue mode">
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{queueMode ?? '--'}</Text>
        </TouchableOpacity>
      </ScrollView>
    </View>
  );
}
