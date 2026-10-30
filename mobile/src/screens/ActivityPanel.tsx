import React, { useEffect, useRef, useState } from 'react';
import { ScrollView, Text, TouchableOpacity, View } from 'react-native';
import { parseJobList } from '../jobCard';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { activityEntryMatchesPage, applyActivityEvent, seedActivityFromList, type ActivityEventFrame, type ActivityPanelPage, type ActivitySessionSnapshot } from '../todoActivity';
import { AppBar, styles, type Palette } from '../ui';

interface Owner { active: boolean; sessionId: string }

export function ActivityPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette } = props;
  const owner = useRef<Owner>({ active: true, sessionId: props.sessionId });
  const [snapshot, setSnapshot] = useState<ActivitySessionSnapshot | null>(null);
  const [page, setPage] = useState<ActivityPanelPage>('recent');
  const [busy, setBusy] = useState(false);
  const onError = useRef(props.onError);
  onError.current = props.onError;
  if (owner.current.sessionId !== props.sessionId) {
    owner.current.active = false;
    owner.current = { active: true, sessionId: props.sessionId };
  }

  useEffect(() => {
    owner.current.active = false;
    const current: Owner = { active: true, sessionId: props.sessionId };
    owner.current = current;
    let pending: ActivityEventFrame[] | null = [];
    setSnapshot(null);
    setPage('recent');
    setBusy(true);
    const off = props.client.subscribeFrames((frame) => {
      if (!frame || typeof frame !== 'object' || Array.isArray(frame)) return;
      const event = frame as ActivityEventFrame;
      if (!current.active || owner.current !== current || event.sessionId !== current.sessionId) return;
      if (event.type !== 'job_updated' && event.type !== 'agent_updated' && event.type !== 'message_delivered') return;
      if (pending !== null) pending.push(event);
      else setSnapshot((previous) => current.active && previous !== null ? applyActivityEvent(previous, event) : previous);
    });
    void (async () => {
      try {
        const response = await props.client.command({ type: 'job_list' }, current.sessionId);
        if (!current.active || owner.current !== current) return;
        if (!response.success) throw new Error(response.error || 'job_list failed');
        const parsed = parseJobList(response.data);
        if (!parsed) throw new Error('invalid job_list');
        const data = response.data as Record<string, unknown>;
        if (data.messages !== undefined && !Array.isArray(data.messages)) throw new Error('invalid job_list messages');
        // parseJobList omits delivered output; retain it for settled Activity summaries.
        const results = new Map((data.jobs as Record<string, unknown>[]).filter((job) => job && typeof job.id === 'string').map((job) => [job.id, job.result]));
        const jobs = parsed.jobs.map((job) => ({ ...job, result: results.get(job.id) }));
        let seeded = seedActivityFromList(current.sessionId, jobs, (data.messages ?? []) as unknown[]);
        for (const frame of pending ?? []) seeded = applyActivityEvent(seeded, frame);
        setSnapshot(seeded);
      } catch (error) {
        if (current.active && owner.current === current) {
          setSnapshot(null);
          onError.current(`Activity: ${error instanceof Error ? error.message : String(error)}`);
        }
      } finally {
        pending = null;
        if (current.active && owner.current === current) setBusy(false);
      }
    })();
    return () => {
      current.active = false;
      current.sessionId = '';
      off();
    };
  }, [props.client, props.sessionId]);

  const entries = snapshot?.sessionId === props.sessionId ? snapshot.entries : [];
  const visible = entries.filter((entry) => activityEntryMatchesPage(entry, page)).slice().reverse();
  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="Activity" onBack={props.onClose} />
      <Text style={[styles.rowSub, { color: palette.muted, padding: 16 }]}>{`${entries.length} ${entries.length === 1 ? 'event' : 'events'}`}</Text>
      <View style={{ flexDirection: 'row' }}>
        {(['recent', 'settled'] as const).map((id) => (
          <TouchableOpacity key={id} accessibilityLabel={id === 'recent' ? 'Recent' : 'Settled'} accessibilityState={{ selected: page === id }} onPress={() => setPage(id)} style={[styles.primaryButton, { flex: 1, marginTop: 0, backgroundColor: page === id ? palette.accent : palette.surface }]}>
            <Text style={[styles.primaryButtonText, { color: page === id ? palette.bg : palette.fg }]}>{id === 'recent' ? 'Recent' : 'Settled'}</Text>
          </TouchableOpacity>
        ))}
      </View>
      <ScrollView style={styles.list}>
        {visible.length === 0 && <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>{busy ? '…' : page === 'recent' ? 'No ordinary activity yet.' : 'No settled ordinary activity yet.'}</Text>}
        {visible.map((entry, index) => (
          <View key={`${entry.id}:${index}`} accessibilityLabel={`Activity ${entry.id}`} style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}>
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]}>{redactSecrets(entry.actor)}</Text>
              <Text style={[styles.rowSub, { color: palette.muted }]}>{`${entry.status} · event ${entry.sequence}`}</Text>
              <Text style={[styles.rowSub, { color: palette.fg }]}>{redactSecrets(entry.summary)}</Text>
            </View>
          </View>
        ))}
      </ScrollView>
    </View>
  );
}
