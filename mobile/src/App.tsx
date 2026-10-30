import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ActivityIndicator, BackHandler, Modal, SafeAreaView, StatusBar, Text, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { parseRunnerInviteLink, type DiscoveredRunner } from './protocol';
import { type RunnerView } from './registry';
import { loadPersistedChannelLogs } from './channelLog';
import { runMappingSync, wireMappingSync } from './mappingSync';
import { lanAddCandidates } from './discovery';
import { lanDiscovery, mappingStore, registry } from './runtime';
import { AddRunnerScreen } from './screens/AddRunnerScreen';
import { ProjectChannelScreen } from './screens/ProjectChannelScreen';
import { ProjectsScreen, NewProjectScreen } from './screens/ProjectsScreen';
import { RunnerDetailScreen } from './screens/RunnerDetailScreen';
import { RunnersScreen } from './screens/RunnersScreen';
import { SideChatScreen } from './screens/SideChatScreen';
import { TmuxTerminalScreen } from './screens/TmuxTerminalScreen';
import { DARK, styles, type Palette } from './ui';
import type { ProjectRecord } from './store';

type HomeTab = 'runners' | 'projects' | 'sidechat';

type Route =
  | { kind: 'home'; tab: HomeTab }
  | { kind: 'runner'; id: string }
  | { kind: 'project'; id: string }
  | { kind: 'add-runner'; address?: string }
  | { kind: 'new-project' }
  | { kind: 'terminal'; runnerId: string; session: string }
  | { kind: 'sidechat'; runnerId: string };

function App(): React.JSX.Element {
  const palette: Palette = DARK;
  const [ready, setReady] = useState(false);
  const [runners, setRunners] = useState<RunnerView[]>([]);
  const [projects, setProjects] = useState<ProjectRecord[]>([]);
  const [discovered, setDiscovered] = useState<DiscoveredRunner[]>([]);
  const [mappingVersion, setMappingVersion] = useState(0);
  const [route, setRoute] = useState<Route>({ kind: 'home', tab: 'runners' });
  const [lastRunnerId, setLastRunnerId] = useState<string | null>(null);
  const [error, setError] = useState('');
  const [bootError, setBootError] = useState('');
  const errorTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const workspaceBack = useRef<{ route: Route; handler: () => boolean } | null>(null);
  const registerWorkspaceBack = useCallback((handler: (() => boolean) | null) => {
    if (handler) workspaceBack.current = { route, handler };
    else if (workspaceBack.current?.route === route) workspaceBack.current = null;
  }, [route]);

  const showError = useCallback((message: string) => {
    if (errorTimer.current) clearTimeout(errorTimer.current);
    setError(message);
    if (message) {
      errorTimer.current = setTimeout(() => setError(''), 5000);
    }
  }, []);

  useEffect(() => {
    let mounted = true;
    const reportRemoteError = (message: string) => {
      if (mounted) showError(message);
    };
    void (async () => {
      // The screens are built from local state, so the local load must finish
      // first — and a local failure must be visible rather than leave the
      // spinner up forever.
      try {
        await registry.load();
        await mappingStore.load();
        await loadPersistedChannelLogs();
      } catch (err) {
        // Local hydration is the prerequisite for every screen: publishing an
        // empty registry as if it were this profile would be a lie, so this is
        // a terminal startup failure shown in place of the spinner.
        if (mounted) setBootError(err instanceof Error ? err.message : String(err));
        return;
      }
      if (!mounted) return;
      registry.connectAll();
      // Wired exactly once, before the boot sync can push: the persist handlers
      // and the module reporter are single slots, and a push must reach the
      // snackbar whether or not the boot round trip ever comes back.
      wireMappingSync(registry, mappingStore, reportRemoteError);
      if (mounted) setReady(true);
      // The remote boot sync is a round trip to a runner that may be offline:
      // it runs after the local UI publishes and never holds it back.
      try {
        await runMappingSync(registry, mappingStore, 'boot');
      } catch (err) {
        reportRemoteError(err instanceof Error ? err.message : String(err));
      }
    })();
    const offRunners = registry.subscribe((views) => {
      if (mounted) setRunners(views);
    });
    const offMappings = mappingStore.subscribe(() => {
      if (mounted) setMappingVersion((v) => v + 1);
    });
    const offLan = lanDiscovery.subscribe(setDiscovered);
    const offBind = lanDiscovery.bind(registry);
    setProjects(mappingStore.projectRows());
    return () => {
      mounted = false;
      offRunners();
      offMappings();
      offLan();
      offBind();
    };
  }, []);

  useEffect(() => {
    setProjects(mappingStore.projectRows());
  }, [mappingVersion]);

  const addedAuthorities = useMemo(() => runners.map((row) => row.authority), [runners]);
  const lanCandidates = useMemo(
    () => lanAddCandidates(discovered, addedAuthorities),
    [addedAuthorities, discovered],
  );

  const addRunner = useCallback(async (raw: string): Promise<boolean> => {
    try {
      const { endpoint, token } = parseRunnerInviteLink(raw);
      const record = await registry.add(endpoint, token, endpoint.authority);
      setRoute({ kind: 'runner', id: record.id });
      setLastRunnerId(record.id);
      return true;
    } catch (err) {
      showError(err instanceof Error ? err.message : String(err));
      return false;
    }
  }, [showError]);

  const addLan = useCallback(async (candidate: DiscoveredRunner) => {
    try {
      const { endpoint, token } = parseRunnerInviteLink(candidate.origin);
      const record = await registry.add(endpoint, token, candidate.codename || candidate.label);
      setRoute({ kind: 'runner', id: record.id });
      setLastRunnerId(record.id);
    } catch (err) {
      showError(err instanceof Error ? err.message : String(err));
    }
  }, [showError]);

  const goHome = useCallback((tab: HomeTab = 'runners') => {
    setRoute({ kind: 'home', tab });
  }, []);

  const goBack = useCallback((): boolean => {
    if (workspaceBack.current?.route === route && workspaceBack.current.handler()) return true;
    switch (route.kind) {
      case 'home':
        return false;
      case 'runner':
      case 'add-runner':
        goHome('runners');
        return true;
      case 'project':
      case 'new-project':
        goHome('projects');
        return true;
      case 'terminal':
      case 'sidechat':
        setRoute({ kind: 'runner', id: route.runnerId });
        return true;
    }
  }, [goHome, route]);

  useEffect(() => {
    const subscription = BackHandler.addEventListener('hardwareBackPress', goBack);
    return () => subscription.remove();
  }, [goBack]);

  const sideChatRunnerId = lastRunnerId
    ?? runners.find((row) => row.state === 'connected')?.id
    ?? runners[0]?.id
    ?? null;
  const sideChatView = runners.find((row) => row.id === sideChatRunnerId);
  const sideChatClient = sideChatRunnerId ? registry.client(sideChatRunnerId) : null;
  const sideChatSessionId = sideChatView?.probe?.sessionId ?? sideChatClient?.probe?.sessionId ?? '';

  if (!ready) {
    return (
      <SafeAreaView style={{ flex: 1, backgroundColor: palette.bg }}>
        {bootError === ''
          ? <ActivityIndicator style={{ marginTop: 64 }} color={palette.accent} />
          : (
            <Text style={[styles.snackbarText, { color: palette.fg, textAlign: 'center', margin: 24 }]}>
              {bootError}
            </Text>
          )}
      </SafeAreaView>
    );
  }

  const errorToast = error === '' ? null : (
    <View style={[styles.snackbar, { backgroundColor: palette.fg }]}>
      <Text style={[styles.snackbarText, { color: palette.bg }]} numberOfLines={2}>{error}</Text>
    </View>
  );

  return (
    <SafeAreaView style={{ flex: 1, backgroundColor: palette.bg }}>
      <StatusBar barStyle="light-content" backgroundColor={palette.surface} />
      {route.kind === 'home' && route.tab === 'runners' && (
        <RunnersScreen
          palette={palette}
          runners={runners}
          lanCandidates={lanCandidates}
          onOpenRunner={(id) => {
            setLastRunnerId(id);
            setRoute({ kind: 'runner', id });
          }}
          onAdd={() => setRoute({ kind: 'add-runner' })}
          onAddLan={(candidate) => { void addLan(candidate); }}
          onError={showError}
        />
      )}
      {route.kind === 'home' && route.tab === 'projects' && (
        <ProjectsScreen
          palette={palette}
          projects={projects}
          onOpenProject={(id) => setRoute({ kind: 'project', id })}
          onNewProject={() => setRoute({ kind: 'new-project' })}
        />
      )}
      {route.kind === 'home' && route.tab === 'sidechat' && (
        <SideChatScreen
          palette={palette}
          client={sideChatClient}
          sessionId={sideChatSessionId}
          onError={showError}
        />
      )}
      {route.kind === 'home' && (
        <View style={[styles.tabBar, { backgroundColor: palette.surface, borderTopColor: palette.border }]}>
          {([
            ['runners', 'dns', 'Runners'],
            ['projects', 'folder-open', 'Projects'],
            ['sidechat', 'chat', 'Side chat'],
          ] as const).map(([tab, icon, label]) => {
            const active = route.tab === tab;
            return (
              <TouchableOpacity
                key={tab}
                style={styles.tabItem}
                onPress={() => setRoute({ kind: 'home', tab })}
                accessibilityLabel={label}
              >
                <Icon
                  name={icon}
                  size={24}
                  color={active ? palette.accent : palette.muted}
                />
                <Text style={[styles.tabLabel, { color: active ? palette.accent : palette.muted }]}>
                  {label}
                </Text>
              </TouchableOpacity>
            );
          })}
        </View>
      )}
      {route.kind === 'runner' && (
        <RunnerDetailScreen
          palette={palette}
          runnerId={route.id}
          runners={runners}
          onRegisterBackHandler={registerWorkspaceBack}
          floatingAction={(
            <TouchableOpacity
              accessibilityLabel="Open side chat"
              style={[styles.fab, { backgroundColor: palette.accent, bottom: 88 }]}
              onPress={() => {
                setLastRunnerId(route.id);
                setRoute({ kind: 'sidechat', runnerId: route.id });
              }}>
              <Icon name="chat" size={24} color={palette.bg} />
            </TouchableOpacity>
          )}
          onBack={goBack}
          onOpenTerminal={(session) => setRoute({ kind: 'terminal', runnerId: route.id, session })}
          onDeleted={() => goHome('runners')}
          onError={showError}
          overlayToast={errorToast}
        />
      )}
      {route.kind === 'sidechat' && (
        <SideChatScreen
          palette={palette}
          client={registry.client(route.runnerId)}
          sessionId={runners.find((row) => row.id === route.runnerId)?.probe?.sessionId
            ?? registry.client(route.runnerId)?.probe?.sessionId
            ?? ''}
          onClose={goBack}
          onError={showError}
        />
      )}
      {route.kind === 'terminal' && (
        <TmuxTerminalScreen
          palette={palette}
          runnerId={route.runnerId}
          session={route.session}
          onClose={goBack}
        />
      )}
      {route.kind === 'project' && (
        <ProjectChannelScreen
          palette={palette}
          projectId={route.id}
          projects={projects}
          runners={runners}
          onRegisterBackHandler={registerWorkspaceBack}
          onBack={goBack}
          onDeleted={() => goHome('projects')}
          onError={showError}
        />
      )}
      {route.kind === 'add-runner' && (
        <AddRunnerScreen
          palette={palette}
          initialAddress={route.address}
          onAdd={addRunner}
          onClose={goBack}
        />
      )}
      {route.kind === 'new-project' && (
        <NewProjectScreen
          palette={palette}
          runners={runners}
          onError={showError}
          onCreated={(projectId) => setRoute({ kind: 'project', id: projectId })}
          onClose={goBack}
        />
      )}
      {errorToast}
    </SafeAreaView>
  );
}

export default App;
