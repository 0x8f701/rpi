import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  ScrollView,
  Text,
  TextInput,
  TouchableOpacity,
  View,
  type NativeSyntheticEvent,
  type TextInputKeyPressEventData,
} from 'react-native';
import { MarkdownBody } from '../MarkdownBody';
import {
  parseSideChatSnapshot,
  type RunnerCommand,
  type SideChatEntryWire,
  type SideChatSnapshotData,
} from '../protocol';
import type { RunnerClient } from '../runnerClient';
import { TranscriptItemView } from '../TranscriptItemView';
import { AppBar, styles, type Palette } from '../ui';

export const SIDE_CHAT_NAME_MAX_CHARS = 32;
const SIDE_CHAT_NAME_ADJECTIVES = [
  'amber', 'brisk', 'coral', 'dusk', 'ember', 'flint', 'grove', 'haze',
  'ivory', 'jade', 'keen', 'lunar', 'maple', 'north', 'olive', 'pearl',
] as const;
const SIDE_CHAT_NAME_NOUNS = [
  'otter', 'willow', 'quartz', 'cedar', 'heron', 'pebble', 'ridge', 'sable',
  'thistle', 'umber', 'violet', 'wren', 'yarrow', 'zephyr', 'aspen', 'birch',
] as const;
export const SIDE_CHAT_RESERVED_NAMES = ['new', 'list', 'close', 'btw'] as const;

function pickSideChatWord(words: readonly string[], random: () => number): string {
  const index = Math.min(words.length - 1, Math.max(0, Math.floor(random() * words.length)));
  return words[index] ?? words[0]!;
}

export function generateSideChatName(
  existingNames: readonly string[],
  random: () => number = Math.random,
): string {
  const taken = new Set(
    [...existingNames, ...SIDE_CHAT_RESERVED_NAMES].map((name) => name.toLowerCase()),
  );
  const adjective = pickSideChatWord(SIDE_CHAT_NAME_ADJECTIVES, random);
  const noun = pickSideChatWord(SIDE_CHAT_NAME_NOUNS, random);
  const base = `${adjective}_${noun}`;
  let candidate = base.length <= SIDE_CHAT_NAME_MAX_CHARS ? base : base.slice(0, SIDE_CHAT_NAME_MAX_CHARS);
  let serial = 2;
  while (taken.has(candidate.toLowerCase())) {
    const suffix = `_${serial}`;
    const stem = base.slice(0, Math.max(1, SIDE_CHAT_NAME_MAX_CHARS - suffix.length));
    candidate = `${stem}${suffix}`;
    serial += 1;
  }
  return candidate;
}

function toolArgsText(args: unknown): string {
  if (args == null) return '';
  if (typeof args === 'string') return args;
  try {
    return JSON.stringify(args);
  } catch {
    return '';
  }
}

function SideChatEntryRow(props: { entry: SideChatEntryWire; palette: Palette }): React.JSX.Element {
  const { entry, palette } = props;
  if (entry.kind === 'tool') {
    return (
      <TranscriptItemView
        item={{
          kind: 'tool',
          id: entry.id,
          toolCallId: entry.id,
          toolName: entry.toolName ?? '',
          args: toolArgsText(entry.toolArgs),
          result: entry.output ?? '',
          status: entry.isPartial ? 'running' : entry.isError ? 'error' : 'done',
        }}
        palette={palette}
      />
    );
  }
  if (entry.kind === 'thinking') {
    return (
      <TranscriptItemView
        item={{
          kind: 'assistant',
          id: entry.id,
          status: entry.isPartial ? 'streaming' : 'final',
          blocks: [{ type: 'thinking', thinking: entry.thinking || entry.text }],
        }}
        palette={palette}
      />
    );
  }
  return (
    <View>
      {entry.kind === 'assistant' && entry.thinking ? (
        <TranscriptItemView
          item={{
            kind: 'assistant',
            id: `${entry.id}-thinking`,
            status: 'final',
            blocks: [{ type: 'thinking', thinking: entry.thinking }],
          }}
          palette={palette}
        />
      ) : null}
      <MarkdownBody text={entry.text} palette={palette} />
    </View>
  );
}

function SideChatTabStrip(props: {
  palette: Palette;
  snapshot: SideChatSnapshotData | null;
  busy: boolean;
  onSwitch: (name: string) => void;
  onCloseTab: (name: string) => void;
  onNew: () => void;
}): React.JSX.Element {
  const { palette, snapshot, busy } = props;
  const tabNames = snapshot?.tabs.map((tab) => tab.name) ?? [];
  const newDisabled = !snapshot || tabNames.length >= (snapshot.maxTabs ?? 0) || busy;
  return (
    <View style={[styles.sideChatTabs, { borderBottomColor: palette.border, backgroundColor: palette.surface }]}>
      {(snapshot?.tabs ?? []).map((tab) => {
        const active = tab.name === snapshot?.active;
        return (
          <View
            key={tab.name}
            style={[styles.sideChatTab, { borderColor: active ? palette.accent : palette.border }]}
          >
            <TouchableOpacity
              onPress={() => props.onSwitch(tab.name)}
              accessibilityLabel={active ? `Active tab ${tab.name}` : `Switch to ${tab.name}`}
              accessibilityState={{ selected: active }}
            >
              <Text style={[styles.sideChatTabLabel, { color: active ? palette.fg : palette.muted }]}>
                {tab.name}{tab.streaming ? ' ●' : ''}
              </Text>
            </TouchableOpacity>
            <TouchableOpacity
              onPress={() => props.onCloseTab(tab.name)}
              accessibilityLabel={`Close tab ${tab.name}`}
            >
              <Text style={[styles.sideChatTabLabel, { color: palette.error }]}>×</Text>
            </TouchableOpacity>
          </View>
        );
      })}
      <TouchableOpacity
        onPress={props.onNew}
        disabled={newDisabled}
        accessibilityLabel="New side-chat tab"
        style={[styles.sideChatTab, { borderColor: palette.border }, newDisabled && styles.faded]}
      >
        <Text style={[styles.sideChatTabLabel, { color: palette.accent }]}>+ New</Text>
      </TouchableOpacity>
    </View>
  );
}

function SideChatComposer(props: {
  palette: Palette;
  draft: string;
  streaming: boolean;
  busy: boolean;
  onChangeText: (text: string) => void;
  onKeyPress: (event: NativeSyntheticEvent<TextInputKeyPressEventData>) => void;
  onSend: () => void;
  onStop: () => void;
}): React.JSX.Element {
  const { palette, draft, streaming, busy } = props;
  return (
    <View style={[styles.composerRow, { backgroundColor: palette.surface, borderTopColor: palette.border }]}>
      <TextInput
        style={[styles.sideChatComposerInput, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
        value={draft}
        onChangeText={props.onChangeText}
        onKeyPress={props.onKeyPress}
        placeholder={streaming ? 'Waiting…' : 'Message'}
        placeholderTextColor={palette.muted}
        multiline
        scrollEnabled
        blurOnSubmit={false}
        accessibilityLabel="Side chat composer"
      />
      {streaming ? (
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.error, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }, busy && styles.faded]}
          disabled={busy}
          onPress={props.onStop}
          accessibilityLabel="Stop">
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>■ Stop</Text>
        </TouchableOpacity>
      ) : (
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }, (draft.trim() === '' || busy) && styles.faded]}
          disabled={draft.trim() === '' || busy}
          onPress={props.onSend}
          accessibilityLabel="Send">
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Send</Text>
        </TouchableOpacity>
      )}
    </View>
  );
}

export function SideChatScreen(props: {
  palette: Palette;
  client: RunnerClient | null;
  sessionId: string;
  onClose?: () => void;
  onError: (message: string) => void;
  random?: () => number;
}): React.JSX.Element {
  const { palette, client, sessionId } = props;
  const [snapshot, setSnapshot] = useState<SideChatSnapshotData | null>(null);
  const [malformed, setMalformed] = useState(false);
  const [draft, setDraft] = useState('');
  const [promptError, setPromptError] = useState('');
  const [busy, setBusy] = useState(false);
  const snapGenRef = useRef(0);
  const busyRef = useRef(false);
  const draftRef = useRef(draft);
  draftRef.current = draft;

  const busyIdRef = useRef(0);

  const claimBusy = (): number | null => {
    if (busyRef.current) return null;
    busyRef.current = true;
    busyIdRef.current += 1;
    setBusy(true);
    return busyIdRef.current;
  };

  const releaseBusy = (id: number) => {
    if (id !== busyIdRef.current) return;
    busyRef.current = false;
    setBusy(false);
  };

  const bumpSnap = (): number => {
    snapGenRef.current += 1;
    if (busyRef.current) {
      busyIdRef.current += 1;
      busyRef.current = false;
      setBusy(false);
    }
    return snapGenRef.current;
  };

  const applySnapshot = useCallback((data: unknown): boolean => {
    const parsed = parseSideChatSnapshot(data);
    if (!parsed) {
      setSnapshot(null);
      setMalformed(true);
      return false;
    }
    setMalformed(false);
    setSnapshot(parsed);
    return true;
  }, []);

  const run = useCallback(async (command: RunnerCommand, gen: number) => {
    if (!client) return null;
    const response = await client.command(command, sessionId || undefined);
    if (gen !== snapGenRef.current) return null;
    return response;
  }, [client, sessionId]);

  useEffect(() => {
    if (!client) {
      setSnapshot(null);
      setMalformed(false);
      return;
    }
    let cancelled = false;
    const load = async () => {
      const gen = snapGenRef.current;
      try {
        const response = await client.command({ type: 'side_chat_list' }, sessionId || undefined);
        if (cancelled || gen !== snapGenRef.current) return;
        if (!response.success) {
          props.onError(response.error || 'side_chat_list failed');
          setSnapshot(null);
          setMalformed(true);
          return;
        }
        applySnapshot(response.data);
      } catch (err) {
        if (cancelled || gen !== snapGenRef.current) return;
        props.onError(err instanceof Error ? err.message : String(err));
        setSnapshot(null);
        setMalformed(true);
      }
    };
    void load();
    const timer = setInterval(() => { void load(); }, 2000);
    return () => {
      cancelled = true;
      bumpSnap();
      clearInterval(timer);
    };
  }, [applySnapshot, client, props.onError, sessionId]);

  const activeTab = snapshot?.tabs.find((tab) => tab.name === snapshot.active) ?? snapshot?.tabs[0] ?? null;
  const streaming = activeTab?.streaming === true;
  const tabNames = snapshot?.tabs.map((tab) => tab.name) ?? [];

  const createTab = async () => {
    if (!client) return;
    const busyId = claimBusy();
    if (busyId == null) return;
    const name = generateSideChatName(tabNames, props.random ?? Math.random);
    const gen = snapGenRef.current + 1;
    snapGenRef.current = gen;
    try {
      const response = await run({ type: 'side_chat_new', name }, gen);
      if (!response) return;
      if (!response.success) {
        props.onError(response.error || 'side_chat_new failed');
        return;
      }
      applySnapshot(response.data);
    } catch (err) {
      if (gen !== snapGenRef.current) return;
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      releaseBusy(busyId);
    }
  };

  const switchTab = async (name: string) => {
    if (!client || name === snapshot?.active) return;
    const gen = bumpSnap();
    try {
      const response = await run({ type: 'side_chat_switch', name }, gen);
      if (!response) return;
      if (!response.success) {
        props.onError(response.error || 'side_chat_switch failed');
        return;
      }
      applySnapshot(response.data);
    } catch (err) {
      if (gen !== snapGenRef.current) return;
      props.onError(err instanceof Error ? err.message : String(err));
    }
  };

  const closeTab = async (name: string) => {
    if (!client) return;
    const gen = bumpSnap();
    try {
      const response = await run({ type: 'side_chat_close', name }, gen);
      if (!response) return;
      if (!response.success) {
        props.onError(response.error || 'side_chat_close failed');
        return;
      }
      applySnapshot(response.data);
    } catch (err) {
      if (gen !== snapGenRef.current) return;
      props.onError(err instanceof Error ? err.message : String(err));
    }
  };

  const submit = async () => {
    const message = draftRef.current.trim();
    if (!client || !message || streaming) return;
    const busyId = claimBusy();
    if (busyId == null) return;
    setPromptError('');
    const gen = snapGenRef.current + 1;
    snapGenRef.current = gen;
    try {
      const response = await run({ type: 'side_chat_prompt', message }, gen);
      if (!response) return;
      if (!response.success) {
        const detail = response.error || 'side_chat_prompt failed';
        setPromptError(detail);
        props.onError(detail);
        return;
      }
      if (!applySnapshot(response.data)) return;
      const parsed = parseSideChatSnapshot(response.data);
      if (parsed?.accepted === false) {
        setPromptError('side chat is busy — wait for the current reply');
        props.onError('side chat is busy — wait for the current reply');
        return;
      }
      setDraft('');
    } catch (err) {
      if (gen !== snapGenRef.current) return;
      const detail = err instanceof Error ? err.message : String(err);
      setPromptError(detail);
      props.onError(detail);
    } finally {
      releaseBusy(busyId);
    }
  };

  const stopReply = async () => {
    if (!client) return;
    const busyId = claimBusy();
    if (busyId == null) return;
    const gen = snapGenRef.current + 1;
    snapGenRef.current = gen;
    try {
      const response = await run({ type: 'side_chat_abort' }, gen);
      if (!response) return;
      if (!response.success) {
        props.onError(response.error || 'side_chat_abort failed');
        return;
      }
      applySnapshot(response.data);
    } catch (err) {
      if (gen !== snapGenRef.current) return;
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      releaseBusy(busyId);
    }
  };

  const onComposerKey = (event: NativeSyntheticEvent<TextInputKeyPressEventData>) => {
    const native = event.nativeEvent as { key?: string; shiftKey?: boolean };
    if (native.key !== 'Enter' || native.shiftKey === true) return;
    void submit();
  };

  if (!client) {
    return (
      <View style={[styles.screen, { backgroundColor: palette.bg }]}>
        <AppBar palette={palette} title="Side chat" onBack={props.onClose} />
        <View style={styles.emptyBox}>
          <Text style={[styles.emptyText, { color: palette.muted }]} accessibilityLabel="Side chat empty">
            Connect a runner to open side chat.
          </Text>
        </View>
      </View>
    );
  }

  const emptyLabel = malformed
    ? 'Side chat snapshot is unreadable.'
    : `No messages yet in “${activeTab?.name || snapshot?.active || 'default'}”.`;

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]} accessibilityLabel="Side chat">
      <AppBar palette={palette} title="Side chat" onBack={props.onClose} />
      <SideChatTabStrip
        palette={palette}
        snapshot={snapshot}
        busy={busy}
        onSwitch={(name) => { void switchTab(name); }}
        onCloseTab={(name) => { void closeTab(name); }}
        onNew={() => { void createTab(); }}
      />
      <ScrollView style={styles.list} keyboardShouldPersistTaps="handled">
        {malformed || !activeTab || activeTab.entries.length === 0 ? (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]} accessibilityLabel="Side chat empty">
            {emptyLabel}
          </Text>
        ) : (
          activeTab.entries.map((entry) => (
            <SideChatEntryRow key={entry.id} entry={entry} palette={palette} />
          ))
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
      {promptError !== '' && (
        <Text style={[styles.inlineError, { color: palette.error }]} accessibilityLabel="Side chat prompt error">
          {promptError}
        </Text>
      )}
      <SideChatComposer
        palette={palette}
        draft={draft}
        streaming={streaming}
        busy={busy}
        onChangeText={setDraft}
        onKeyPress={onComposerKey}
        onSend={() => { void submit(); }}
        onStop={() => { void stopReply(); }}
      />
    </View>
  );
}
