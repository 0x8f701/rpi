import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { AppState, ScrollView, Text, TextInput, TouchableOpacity, View, type AppStateStatus } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import {
  appendChannelLog,
  channelLogsFor,
  persistChannelLogs,
  type ChannelLogMessage,
} from '../channelLog';
import {
  insertMentionToken,
  resolveChannelRecipients,
  runnerMentionToken,
  trailingMentionQuery,
  type ChannelMentionMember,
} from '../chatMentions';
import {
  parseMessages,
  parseSessionCatalog,
  type RuntimeIdentity,
} from '../protocol';
import { runnerViewName, type RunnerView } from '../registry';
import { mappingStore, registry } from '../runtime';
import type { MappingRecord, ProjectRecord } from '../store';
import { isClosedTransportFailure } from '../transportError';
import { applySessionEvent, channelTerminalReplyText, emptyTranscript, messagesToItems, type TranscriptItem, type TranscriptState } from '../transcriptItems';
import { planTurnStreaming } from '../turnStreaming';
import { TranscriptItemView } from '../TranscriptItemView';
import { MarkdownBody } from '../MarkdownBody';
import { AppBar, styles, type Palette } from '../ui';
import type { RunnerClient } from '../runnerClient';

function mentionOf(runner: RunnerView): string {
  return runnerMentionToken(runnerViewName(runner));
}

async function resolveMappingIdentity(client: RunnerClient, mapping: MappingRecord): Promise<RuntimeIdentity | null> {
  const response = await client.command({ type: 'session_list', scope: 'all_projects' });
  const rows = response.success ? parseSessionCatalog(response.data) : null;
  const row = rows?.find((item) => item.sessionId === mapping.sessionId);
  if (row?.runtimeIncarnation) {
    return { sessionId: row.sessionId, runtimeIncarnation: row.runtimeIncarnation };
  }
  if (client.probe?.sessionId === mapping.sessionId && client.probe.runtimeIncarnation) {
    return { sessionId: mapping.sessionId, runtimeIncarnation: client.probe.runtimeIncarnation };
  }
  return null;
}

function frameSessionId(frame: unknown): string {
  if (!frame || typeof frame !== 'object') return '';
  const value = frame as Record<string, unknown>;
  return typeof value.sessionId === 'string' ? value.sessionId : '';
}

function frameType(frame: unknown): string {
  if (!frame || typeof frame !== 'object') return '';
  const value = frame as Record<string, unknown>;
  return typeof value.type === 'string' ? value.type : '';
}

export function ProjectChannelScreen(props: {
  palette: Palette;
  projectId: string;
  projects: ProjectRecord[];
  runners: RunnerView[];
  onBack: () => void;
  onDeleted: () => void;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
}): React.JSX.Element {
  const { palette } = props;
  const project = props.projects.find((row) => row.id === props.projectId);
  const [mapTick, setMapTick] = useState(0);
  useEffect(() => mappingStore.subscribe(() => setMapTick((value) => value + 1)), []);
  const mappings = useMemo(
    () => mappingStore.mappingsForProject(props.projectId),
    [props.projectId, mapTick],
  );
  const mappingKey = mappings.map((mapping) => `${mapping.id}:${mapping.sessionId}`).join('|');
  const [logs, setLogs] = useState<ChannelLogMessage[]>(() => channelLogsFor(props.projectId));
  const [cards, setCards] = useState<Record<string, TranscriptState>>({});
  const [streaming, setStreaming] = useState<Record<string, boolean>>({});
  const [draft, setDraft] = useState('');
  const [sending, setSending] = useState(false);
  const sendingRef = useRef(false);
  const [deleteArmed, setDeleteArmed] = useState(false);
  const consumeBack = useCallback((): boolean => {
    if (!deleteArmed) return false;
    setDeleteArmed(false);
    return true;
  }, [deleteArmed]);
  const registerBackHandler = props.onRegisterBackHandler;
  useEffect(() => {
    registerBackHandler?.(consumeBack);
    return () => registerBackHandler?.(null);
  }, [registerBackHandler, consumeBack]);
  const identitiesRef = useRef<Record<string, RuntimeIdentity>>({});
  const streamingRef = useRef<Record<string, boolean>>({});
  const turnPriorAssistantIdsRef = useRef<Record<string, string[]>>({});
  useEffect(() => {
    setDeleteArmed(false);
  }, [props.projectId]);

  const members: ChannelMentionMember[] = useMemo(() => {
    const rows: ChannelMentionMember[] = [];
    for (const mapping of mappings) {
      const runner = props.runners.find((row) => row.id === mapping.runnerId);
      if (!runner) continue;
      rows.push({ mappingId: mapping.id, mention: mentionOf(runner) });
    }
    return rows;
  }, [mappings, props.runners]);

  const mentionQuery = trailingMentionQuery(draft);
  const mentionChoices = mentionQuery === null
    ? []
    : mappings.filter((mapping) => {
      const runner = props.runners.find((row) => row.id === mapping.runnerId);
      if (!runner) return false;
      return mentionOf(runner).toLowerCase().startsWith(mentionQuery.toLowerCase());
    });

  const commitLog = useCallback((message: ChannelLogMessage) => {
    const next = appendChannelLog(props.projectId, message);
    setLogs(next);
    void persistChannelLogs();
  }, [props.projectId]);

  const attachMapping = useCallback(async (mapping: MappingRecord) => {
    const client = registry.client(mapping.runnerId);
    if (!client || client.state !== 'connected') return;
    const identity = await resolveMappingIdentity(client, mapping);
    if (!identity) return;
    identitiesRef.current[mapping.id] = identity;
    client.subscribe(identity);
    const messages = await client.command({ type: 'get_messages' }, identity.sessionId);
    const list = messages.success ? parseMessages(messages.data) : null;
    const items = list ? messagesToItems(list) : [];
    delete turnPriorAssistantIdsRef.current[mapping.id];
    setCards((current) => ({
      ...current,
      [mapping.id]: { items, activeAssistantId: null },
    }));
  }, []);

  useEffect(() => {
    setLogs(channelLogsFor(props.projectId));
    let mounted = true;
    void (async () => {
      for (const mapping of mappings) {
        try {
          await attachMapping(mapping);
        } catch (err) {
          if (mounted) props.onError(err instanceof Error ? err.message : String(err));
        }
      }
    })();
    const offs: Array<() => void> = [];
    const runnerIds = [...new Set(mappings.map((mapping) => mapping.runnerId))];
    for (const runnerId of runnerIds) {
      const client = registry.client(runnerId);
      if (!client) continue;
      offs.push(client.subscribeFrames((frame) => {
        if (!mounted) return;
        const sessionId = frameSessionId(frame);
        const mapping = mappings.find((row) => row.sessionId === sessionId);
        if (!mapping) return;
        const type = frameType(frame);
        if (type === 'todo_updated' || type === 'goal_updated') return;
        const activeTurnId = (frame as Record<string, unknown>).activeTurnId;
        const currentLive = streamingRef.current[mapping.id] === true;
        const nextLive = planTurnStreaming(
          type,
          currentLive,
          typeof activeTurnId === 'string' ? activeTurnId : undefined,
        );
        let wasStreaming = false;
        if (type === 'turn_terminal') {
          wasStreaming = currentLive;
        }
        if (nextLive !== currentLive) {
          streamingRef.current = { ...streamingRef.current, [mapping.id]: nextLive };
          setStreaming((current) => ({ ...current, [mapping.id]: nextLive }));
        }
        setCards((current) => {
          const prior = current[mapping.id] ?? emptyTranscript();
          if ((type === 'message_start' || type === 'message_update' || type === 'subscription_snapshot' || type === 'session_resync' || type === 'turn_started' || type === 'turn_start')
            && turnPriorAssistantIdsRef.current[mapping.id] === undefined) {
            turnPriorAssistantIdsRef.current[mapping.id] = prior.items
              .filter((item: TranscriptItem) => item.kind === 'assistant')
              .map((item) => item.id);
          }
          const next = applySessionEvent(prior, frame);
          if (type === 'message_start' && next.activeAssistantId) {
            const baseline = turnPriorAssistantIdsRef.current[mapping.id];
            if (baseline) {
              turnPriorAssistantIdsRef.current[mapping.id] = baseline.filter((id) => id !== next.activeAssistantId);
            }
          }
          if (type === 'turn_terminal' && wasStreaming) {
            const priorIds = new Set(turnPriorAssistantIdsRef.current[mapping.id] ?? []);
            const text = channelTerminalReplyText(frame, next.items, priorIds);
            if (text) {
              const runner = props.runners.find((row) => row.id === mapping.runnerId);
              commitLog({
                id: `r-${mapping.id}-${Date.now()}`,
                at: Date.now(),
                kind: 'runner',
                mappingId: mapping.id,
                author: runner ? mentionOf(runner) : mapping.runnerId,
                text,
              });
            }
          }
          if (type === 'turn_terminal') {
            delete turnPriorAssistantIdsRef.current[mapping.id];
          }
          return { ...current, [mapping.id]: next };
        });
      }));
      offs.push(client.subscribeReconnect(() => {
        const owned = mappings.filter((mapping) => mapping.runnerId === runnerId);
        for (const mapping of owned) {
          void attachMapping(mapping).catch((err) => {
            if (mounted) props.onError(err instanceof Error ? err.message : String(err));
          });
        }
      }));
    }
    const appState = AppState.addEventListener('change', (next: AppStateStatus) => {
      if (next !== 'active' || !mounted) return;
      for (const runnerId of runnerIds) {
        const client = registry.client(runnerId);
        if (!client) continue;
        if (!client.hasOpenSocket()) {
          client.resumeFromBackground();
          continue;
        }
        const owned = mappings.filter((mapping) => mapping.runnerId === runnerId);
        for (const mapping of owned) {
          void attachMapping(mapping).catch((err) => {
            if (isClosedTransportFailure(err)) {
              client.resumeFromBackground();
              return;
            }
            props.onError(err instanceof Error ? err.message : String(err));
          });
        }
      }
    });
    return () => {
      mounted = false;
      appState.remove();
      for (const off of offs) off();
    };
  }, [attachMapping, commitLog, mappingKey, props.onError, props.projectId, props.runners]);

  if (!project) {
    return (
      <View style={[styles.screen, { backgroundColor: palette.bg }]}>
        <AppBar palette={palette} title="Project" onBack={props.onBack} />
        <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>Project was removed.</Text>
      </View>
    );
  }

  const send = async () => {
    const message = draft.trim();
    if (message === '' || sendingRef.current) return;
    if (message.startsWith('/')) {
      props.onError('Open a runner session for slash commands.');
      return;
    }
    const plan = resolveChannelRecipients(message, members);
    if (plan.kind === 'unmatched') {
      props.onError(`No runner named @${plan.token}.`);
      return;
    }
    if (plan.kind === 'ambiguous') {
      props.onError(`@${plan.token} matches more than one runner.`);
      return;
    }
    const targets = plan.kind === 'mentioned'
      ? mappings.filter((mapping) => plan.mappingIds.includes(mapping.id))
      : mappings;
    if (targets.length === 0) {
      props.onError('Add a runner to join this channel.');
      return;
    }
    sendingRef.current = true;
    setSending(true);
    setDraft('');
    try {
      commitLog({
        id: `u-${Date.now()}`,
        at: Date.now(),
        kind: 'human',
        mappingId: null,
        author: 'You',
        text: message,
      });
      for (const mapping of targets) {
        const runner = props.runners.find((row) => row.id === mapping.runnerId);
        const client = registry.client(mapping.runnerId);
        const name = runner ? runnerViewName(runner) : mapping.runnerId;
        if (!client || client.state !== 'connected') {
          commitLog({
            id: `r-${mapping.id}-${Date.now()}`,
            at: Date.now(),
            kind: 'runner',
            mappingId: mapping.id,
            author: name,
            text: 'Runner is not connected.',
          });
          continue;
        }
        try {
          const response = await client.command({ type: 'prompt', message }, mapping.sessionId);
          if (!response.success) {
            commitLog({
              id: `r-${mapping.id}-${Date.now()}`,
              at: Date.now(),
              kind: 'runner',
              mappingId: mapping.id,
              author: name,
              text: response.error || 'Prompt failed',
            });
          } else {
            if (turnPriorAssistantIdsRef.current[mapping.id] === undefined) {
              turnPriorAssistantIdsRef.current[mapping.id] = (cards[mapping.id]?.items ?? [])
                .filter((item) => item.kind === 'assistant')
                .map((item) => item.id);
            }
            streamingRef.current = { ...streamingRef.current, [mapping.id]: true };
            setStreaming((current) => ({ ...current, [mapping.id]: true }));
          }
        } catch (err) {
          commitLog({
            id: `r-${mapping.id}-${Date.now()}`,
            at: Date.now(),
            kind: 'runner',
            mappingId: mapping.id,
            author: name,
            text: err instanceof Error ? err.message : String(err),
          });
        }
      }
    } finally {
      sendingRef.current = false;
      setSending(false);
    }
  };

  const liveCards = mappings.filter((mapping) => {
    const card = cards[mapping.id];
    return (card && card.items.length > 0) || streaming[mapping.id] === true;
  });

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title={project.label}
        onBack={() => {
          if (!consumeBack()) props.onBack();
        }}
        actions={[{
          icon: 'delete-outline',
          label: deleteArmed ? 'Delete?' : 'Delete project',
          onPress: () => {
            if (!deleteArmed) {
              setDeleteArmed(true);
              return;
            }
            void mappingStore.deleteProject(registry, project.id).then(props.onDeleted, (err) => {
              props.onError(err instanceof Error ? err.message : String(err));
            });
          },
        }]}
      />
      <ScrollView style={styles.list}>
        {logs.length === 0 && liveCards.length === 0 && (
          <View style={styles.emptyBox}>
            <Icon name="forum" size={40} color={palette.muted} />
            <Text style={[styles.emptyText, { color: palette.muted }]}>Send a message to mapped runners.</Text>
          </View>
        )}
        {logs.map((row) => (
          <View
            key={row.id}
            style={[
              row.kind === 'human' ? styles.bubbleUser : styles.bubbleRunner,
              { backgroundColor: palette.surface, borderColor: palette.border },
            ]}>
            <Text style={[styles.rowSub, { color: palette.muted }]}>
              {row.kind === 'human' ? 'You' : `@${row.author}`}
            </Text>
            <MarkdownBody text={row.text} palette={palette} />
          </View>
        ))}
        {liveCards.map((mapping) => {
          const runner = props.runners.find((row) => row.id === mapping.runnerId);
          const card = cards[mapping.id] ?? emptyTranscript();
          const isStreaming = streaming[mapping.id] === true;
          return (
            <View
              key={mapping.id}
              style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border, marginHorizontal: 12, marginTop: 8 }]}>
              <View style={styles.cardHead}>
                <Text style={[styles.cardTitle, { color: palette.fg }]} numberOfLines={1}>
                  {runner ? runnerViewName(runner) : mapping.runnerId}
                </Text>
                {isStreaming && (
                  <TouchableOpacity
                    onPress={async () => {
                      const client = registry.client(mapping.runnerId);
                      if (!client) return;
                      try {
                        const response = await client.command({ type: 'abort' }, mapping.sessionId);
                        if (!response.success) {
                          props.onError(response.error || 'Stop failed');
                        }
                      } catch (err) {
                        props.onError(err instanceof Error ? err.message : String(err));
                      }
                    }}
                    accessibilityLabel="Stop">
                    <Text style={{ color: palette.error, fontSize: 18 }}>■</Text>
                  </TouchableOpacity>
                )}
              </View>
              {card.items.map((item) => (
                <TranscriptItemView key={item.id} item={item} palette={palette} />
              ))}
            </View>
          );
        })}
        <View style={styles.bottomPad} />
      </ScrollView>
      {mentionChoices.length > 0 && (
        <View style={{ backgroundColor: palette.surface, borderTopColor: palette.border, borderTopWidth: 1 }}>
          {mentionChoices.map((mapping) => {
            const runner = props.runners.find((row) => row.id === mapping.runnerId);
            if (!runner) return null;
            const token = mentionOf(runner);
            return (
              <TouchableOpacity
                key={mapping.id}
                style={styles.listRow}
                onPress={() => setDraft(insertMentionToken(draft, token))}
                accessibilityLabel={`Mention ${token}`}>
                <Text style={[styles.rowTitle, { color: palette.accent }]}>@{token}</Text>
              </TouchableOpacity>
            );
          })}
        </View>
      )}
      <View style={[styles.composerRow, { backgroundColor: palette.surface, borderTopColor: palette.border }]}>
        <TextInput
          style={[styles.composerInput, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
          value={draft}
          onChangeText={setDraft}
          placeholder="Message"
          placeholderTextColor={palette.muted}
          editable={!sending}
        />
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent, marginTop: 0, paddingVertical: 10, paddingHorizontal: 14 }, (draft.trim() === '' || sending) && styles.faded]}
          disabled={draft.trim() === '' || sending}
          onPress={() => void send()}
          accessibilityLabel="Send">
          <Icon name="send" size={20} color={palette.bg} />
        </TouchableOpacity>
      </View>
    </View>
  );
}
