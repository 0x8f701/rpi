import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  Modal,
  RefreshControl,
  ScrollView,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native';
import { AnsiText } from '../AnsiText';
import { parseTmuxPaneSnapshot, parseTmuxSessionList, type TmuxPaneSnapshotData, type TmuxSessionRow } from '../protocol';
import { runnerViewName, type RunnerView } from '../registry';
import { mappingStore, registry } from '../runtime';
import { SessionWorkspaceScreen } from './SessionWorkspaceScreen';
import { TokenEntryModal } from './TokenEntryModal';
import { TrustCertificate } from '../TrustCertificate';
import { AppBar, StatusDot, styles, type Palette } from '../ui';

const TMUX_PREVIEW_LINES = 24;
const TMUX_STATUS_UNAVAILABLE = 'Tmux status unavailable';
const TMUX_NO_SERVER = 'No tmux server on this runner';
const TMUX_NO_SESSIONS = 'No tmux sessions';

export function RunnerDetailScreen(props: {
  palette: Palette;
  runnerId: string;
  runners: RunnerView[];
  onBack: () => void;
  onOpenTerminal: (session: string) => void;
  onDeleted: () => void;
  onError: (message: string) => void;
  onRegisterBackHandler?: (handler: (() => boolean) | null) => void;
  floatingAction?: React.ReactNode;
  overlayToast?: React.ReactNode;
}): React.JSX.Element {
  const { palette } = props;
  const runner = props.runners.find((row) => row.id === props.runnerId);
  const [sessions, setSessions] = useState<TmuxSessionRow[] | null>(null);
  const [statusLine, setStatusLine] = useState('');
  const [panes, setPanes] = useState<Record<string, TmuxPaneSnapshotData | null | undefined>>({});
  const [refreshing, setRefreshing] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const [createName, setCreateName] = useState('');
  const [tokenOpen, setTokenOpen] = useState(false);
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  const [runnerDeleteArmed, setRunnerDeleteArmed] = useState(false);
  const commitRunnerDelete = () => {
    if (!runnerDeleteArmed) {
      setRunnerDeleteArmed(true);
      return;
    }
    if (!runner) return;
    void mappingStore.removeRunner(registry, runner.id).then(props.onDeleted, (err) => {
      props.onError(err instanceof Error ? err.message : String(err));
    });
  };
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [surface, setSurface] = useState<'terminals' | 'session'>('terminals');
  const refreshSeqRef = useRef(0);
  const workspaceBack = useRef<{
    owner: (handler: (() => boolean) | null) => void;
    handler: () => boolean;
  } | null>(null);
  const registerWorkspaceBack = useCallback(function register(handler: (() => boolean) | null) {
    if (handler) workspaceBack.current = { owner: register, handler };
    else if (workspaceBack.current?.owner === register) workspaceBack.current = null;
  }, [props.runnerId, surface]);
  const consumeBack = useCallback((): boolean => {
    if (runnerDeleteArmed) {
      setRunnerDeleteArmed(false);
      return true;
    }
    return surface === 'session'
      && workspaceBack.current?.owner === registerWorkspaceBack
      && workspaceBack.current.handler();
  }, [runnerDeleteArmed, surface, registerWorkspaceBack]);
  const registerBackHandler = props.onRegisterBackHandler;
  useEffect(() => {
    registerBackHandler?.(consumeBack);
    return () => registerBackHandler?.(null);
  }, [registerBackHandler, consumeBack]);

  useEffect(() => {
    setRunnerDeleteArmed(false);
  }, [props.runnerId, surface]);

  const refresh = useCallback(async () => {
    const seq = ++refreshSeqRef.current;
    const client = registry.client(props.runnerId);
    if (!client || client.state !== 'connected' || !client.probe?.sessionId) {
      if (seq !== refreshSeqRef.current) return;
      setStatusLine(TMUX_STATUS_UNAVAILABLE);
      setSessions(null);
      return;
    }
    try {
      const response = await client.command({ type: 'tmux_list_sessions' });
      const parsed = response.success ? parseTmuxSessionList(response.data) : null;
      if (!parsed) {
        if (seq !== refreshSeqRef.current) return;
        setStatusLine(TMUX_STATUS_UNAVAILABLE);
        return;
      }
      if (!parsed.tmuxAvailable) {
        if (seq !== refreshSeqRef.current) return;
        setSessions([]);
        setStatusLine(TMUX_NO_SERVER);
        return;
      }
      const rows = [...parsed.sessions].sort((left, right) => left.name.localeCompare(right.name));
      if (seq !== refreshSeqRef.current) return;
      setSessions(rows);
      setStatusLine(rows.length === 0 ? TMUX_NO_SESSIONS : '');
      setPanes(Object.fromEntries(rows.map((row) => [row.name, undefined])));
      await Promise.all(rows.map(async (row) => {
        try {
          const snap = await client.command({ type: 'tmux_pane_snapshot', session: row.name, lines: TMUX_PREVIEW_LINES });
          const frame = snap.success ? parseTmuxPaneSnapshot(snap.data) : null;
          if (seq !== refreshSeqRef.current) return;
          setPanes((prev) => ({ ...prev, [row.name]: frame }));
        } catch {
          if (seq !== refreshSeqRef.current) return;
          setPanes((prev) => ({ ...prev, [row.name]: null }));
        }
      }));
    } catch {
      if (seq !== refreshSeqRef.current) return;
      setStatusLine(TMUX_STATUS_UNAVAILABLE);
    }
  }, [props.runnerId]);

  useEffect(() => {
    void refresh();
  }, [refresh, runner?.state]);

  const createSession = async () => {
    const name = createName.trim();
    if (name === '') return;
    const client = registry.client(props.runnerId);
    if (!client) return;
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      const response = await client.command({ type: 'tmux_session_create', session: name });
      if (!response.success) {
        props.onError(response.error || 'Create failed');
        return;
      }
      setCreateOpen(false);
      setCreateName('');
      await refresh();
    } catch (err) {
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  const deleteSession = async (name: string) => {
    if (pendingDelete !== name) {
      setPendingDelete(name);
      return;
    }
    const client = registry.client(props.runnerId);
    if (!client) return;
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      const response = await client.command({ type: 'tmux_session_delete', session: name });
      if (!response.success) props.onError(response.error || 'Delete failed');
    } catch (err) {
      props.onError(err instanceof Error ? err.message : 'Delete failed');
    } finally {
      setPendingDelete(null);
      busyRef.current = false;
      setBusy(false);
      await refresh();
    }
  };

  if (!runner) {
    return (
      <View style={[styles.screen, { backgroundColor: palette.bg }]}>
        <AppBar palette={palette} title="Runner" onBack={props.onBack} />
        <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>Runner was removed.</Text>
      </View>
    );
  }

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title={runnerViewName(runner)}
        leading={<StatusDot state={runner.state} palette={palette} />}
        onBack={() => {
          if (!consumeBack()) props.onBack();
        }}
        actions={surface === 'terminals' ? [
          { icon: 'add', label: 'Create session', onPress: () => setCreateOpen(true), disabled: runner.state !== 'connected' },
          { icon: 'vpn-key', label: 'Set token', onPress: () => setTokenOpen(true) },
          {
            icon: 'delete-outline',
            label: runnerDeleteArmed ? 'Delete?' : 'Delete runner',
            onPress: commitRunnerDelete,
          },
        ] : [
          { icon: 'vpn-key', label: 'Set token', onPress: () => setTokenOpen(true) },
          {
            icon: 'delete-outline',
            label: runnerDeleteArmed ? 'Delete?' : 'Delete runner',
            onPress: commitRunnerDelete,
          },
        ]}
      />
      <View style={[styles.tabBar, { backgroundColor: palette.surface, borderTopWidth: 0, borderBottomWidth: 1, borderBottomColor: palette.border }]}>
        <TouchableOpacity style={styles.tabItem} onPress={() => setSurface('terminals')} accessibilityLabel="Terminals">
          <Text style={[styles.tabLabel, { color: surface === 'terminals' ? palette.accent : palette.muted }]}>Terminals</Text>
        </TouchableOpacity>
        <TouchableOpacity style={styles.tabItem} onPress={() => setSurface('session')} accessibilityLabel="Session">
          <Text style={[styles.tabLabel, { color: surface === 'session' ? palette.accent : palette.muted }]}>Session</Text>
        </TouchableOpacity>
      </View>
      {surface === 'session' ? (
        <SessionWorkspaceScreen
          palette={palette}
          runnerId={props.runnerId}
          onError={props.onError}
          onRegisterBackHandler={registerWorkspaceBack}
          floatingAction={props.floatingAction}
          overlayToast={props.overlayToast}
        />
      ) : (
      <>
      <ScrollView
        style={styles.list}
        refreshControl={(
          <RefreshControl
            refreshing={refreshing}
            colors={[palette.accent]}
            tintColor={palette.accent}
            onRefresh={() => {
              setRefreshing(true);
              void refresh().finally(() => setRefreshing(false));
            }}
          />
        )}>
        {runner.state !== 'connected' && (
          <View style={styles.emptyBox}>
            <StatusDot state={runner.state} palette={palette} />
            {runner.origin.startsWith('https://') && (
              <TrustCertificate
                origin={runner.origin}
                palette={palette}
                onError={props.onError}
                onTrusted={() => {
                  const client = registry.client(runner.id);
                  if (client) client.connect();
                  else registry.connect(runner.id);
                }}
              />
            )}
            <TouchableOpacity onPress={() => {
              const client = registry.client(runner.id);
              if (client) client.connect();
              else registry.connect(runner.id);
            }} accessibilityLabel="Reconnect">
              <Text style={[styles.emptyText, { color: palette.accent }]}>Reconnect</Text>
            </TouchableOpacity>
          </View>
        )}
        {statusLine !== '' && runner.state === 'connected' && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 16 }]}>{statusLine}</Text>
        )}
        {sessions?.map((row) => {
          const pane = panes[row.name];
          return (
            <TouchableOpacity
              key={row.name}
              style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border }]}
              onPress={() => props.onOpenTerminal(row.name)}
              accessibilityLabel={`Open terminal ${row.name}`}>
              <View style={styles.cardHead}>
                <Text style={[styles.cardTitle, { color: palette.fg }]} numberOfLines={1}>{row.name}</Text>
                {row.attached && <Text style={[styles.cardChip, { borderColor: palette.border, color: palette.muted }]}>attached</Text>}
                <Text style={[styles.cardMeta, { color: palette.muted }]}>{row.windows === 1 ? '1 window' : `${row.windows} windows`}</Text>
                <TouchableOpacity
                  accessibilityLabel={pendingDelete === row.name ? `Confirm delete ${row.name}` : `Delete session ${row.name}`}
                  onPress={(event) => {
                    event.stopPropagation();
                    void deleteSession(row.name);
                  }}
                  hitSlop={{ top: 8, bottom: 8, left: 8, right: 8 }}>
                  <Text style={[styles.cardMeta, { color: pendingDelete === row.name ? palette.error : palette.muted }]}>
                    {pendingDelete === row.name ? 'Delete?' : '×'}
                  </Text>
                </TouchableOpacity>
              </View>
              {pane === undefined ? (
                <Text style={[styles.cardPre, { color: palette.muted, fontFamily: palette.mono }]}>--</Text>
              ) : pane === null ? (
                <Text style={[styles.emptyText, { color: palette.muted }]}>{TMUX_STATUS_UNAVAILABLE}</Text>
              ) : (
                <AnsiText text={pane.content} numberOfLines={24} style={[styles.cardPre, { color: palette.fg, fontFamily: palette.mono }]} />
              )}
            </TouchableOpacity>
          );
        })}
        <View style={styles.bottomPad} />
      </ScrollView>
      {props.floatingAction}
      <Modal visible={createOpen} transparent animationType="fade" onRequestClose={() => setCreateOpen(false)}>
        <View style={styles.modalScrim}>
          <View style={[styles.modalCard, { backgroundColor: palette.surface, borderColor: palette.border }]}>
            <Text style={[styles.rowTitle, { color: palette.fg }]}>New tmux session</Text>
            <TextInput
              style={[styles.input, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
              value={createName}
              onChangeText={setCreateName}
              placeholder="session name"
              placeholderTextColor={palette.muted}
              autoCapitalize="none"
              autoCorrect={false}
            />
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, (createName.trim() === '' || busy) && styles.faded]}
              disabled={createName.trim() === '' || busy}
              onPress={() => void createSession()}>
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Create</Text>
            </TouchableOpacity>
            <TouchableOpacity onPress={() => setCreateOpen(false)} accessibilityLabel="Close">
              <Text style={[styles.emptyText, { color: palette.muted }]}>Close</Text>
            </TouchableOpacity>
          </View>
          {props.overlayToast}
        </View>
      </Modal>
      </>
      )}
      <TokenEntryModal
        visible={tokenOpen}
        palette={palette}
        busy={busy}
        onSave={(token) => {
          void (async () => {
            if (busyRef.current) return;
            busyRef.current = true;
            setBusy(true);
            try {
              await registry.updateToken(runner.id, token);
              setTokenOpen(false);
            } catch (err) {
              props.onError(err instanceof Error ? err.message : String(err));
            } finally {
              busyRef.current = false;
              setBusy(false);
            }
          })();
        }}
        onCancel={() => setTokenOpen(false)}
        overlayToast={props.overlayToast}
      />
    </View>
  );
}
