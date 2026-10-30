import React, { useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import type { ProjectRecord } from '../store';
import { mappingStore, registry } from '../runtime';
import { runnerViewName, type RunnerView } from '../registry';
import { AppBar, StatusDot, styles, type Palette } from '../ui';

export function ProjectsScreen(props: {
  palette: Palette;
  projects: ProjectRecord[];
  onOpenProject: (id: string) => void;
  onNewProject: () => void;
}): React.JSX.Element {
  const { palette } = props;
  return (
    <View style={styles.screen}>
      <AppBar palette={palette} title="Projects" />
      <ScrollView style={styles.list}>
        {props.projects.length === 0 && (
          <View style={styles.emptyBox}>
            <Icon name="folder-open" size={40} color={palette.muted} />
            <Text style={[styles.emptyText, { color: palette.muted }]}>Create a project to open a channel.</Text>
          </View>
        )}
        {props.projects.map((project) => (
          <TouchableOpacity
            key={project.id}
            style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
            onPress={() => props.onOpenProject(project.id)}>
            <Icon name="tag" size={20} color={palette.accent} />
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{project.label}</Text>
              <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={1}>{project.cwd}</Text>
            </View>
            <Icon name="chevron-right" size={24} color={palette.muted} />
          </TouchableOpacity>
        ))}
        <View style={styles.bottomPad} />
      </ScrollView>
      <TouchableOpacity
        style={[styles.fab, { backgroundColor: palette.accent }]}
        onPress={props.onNewProject}
        accessibilityLabel="New project">
        <Icon name="add" size={28} color={palette.bg} />
      </TouchableOpacity>
    </View>
  );
}

export function NewProjectScreen(props: {
  palette: Palette;
  runners: RunnerView[];
  onError: (message: string) => void;
  onCreated: (projectId: string) => void;
  onClose: () => void;
}): React.JSX.Element {
  const { palette } = props;
  const [label, setLabel] = useState('');
  const [cwd, setCwd] = useState('');
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);

  const connected = props.runners.filter((runner) => runner.state === 'connected');

  const toggle = (id: string) => {
    setSelected((prev) => (prev.includes(id) ? prev.filter((row) => row !== id) : [...prev, id]));
  };

  const submit = async () => {
    if (!cwd.trim().startsWith('/')) {
      props.onError('Project directory must be absolute.');
      return;
    }
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      const project = await mappingStore.addProject(label, cwd);
      for (const runnerId of selected) {
        try {
          await mappingStore.mapRunner(registry, project.id, runnerId);
        } catch (err) {
          props.onError(err instanceof Error ? err.message : String(err));
        }
      }
      props.onCreated(project.id);
    } catch (err) {
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="New project" onBack={props.onClose} />
      <ScrollView contentContainerStyle={styles.form}>
        <TextInput
          style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg, minHeight: 44 }]}
          value={label}
          onChangeText={setLabel}
          placeholder="Label"
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
        />
        <TextInput
          style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg, minHeight: 44 }]}
          value={cwd}
          onChangeText={setCwd}
          placeholder="/workspace/project"
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
          autoCorrect={false}
        />
        <Text style={[styles.formSection, { color: palette.muted }]}>Runners in this project</Text>
        {connected.length === 0 && (
          <Text style={[styles.emptyText, { color: palette.muted }]}>Connect a runner first.</Text>
        )}
        {connected.map((runner) => {
          const checked = selected.includes(runner.id);
          return (
            <TouchableOpacity
              key={runner.id}
              style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
              onPress={() => toggle(runner.id)}>
              <Icon
                name={checked ? 'check-box' : 'check-box-outline-blank'}
                size={24}
                color={checked ? palette.accent : palette.muted}
              />
              <StatusDot state={runner.state} palette={palette} />
              <View style={styles.rowBody}>
                <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{runnerViewName(runner)}</Text>
              </View>
            </TouchableOpacity>
          );
        })}
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent }, (label.trim() === '' || !cwd.trim().startsWith('/') || busy) && styles.faded]}
          disabled={label.trim() === '' || cwd.trim() === '' || busy}
          accessibilityLabel="Confirm"
          onPress={() => void submit()}>
          <Icon name="check" size={20} color={palette.bg} />
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{busy ? 'Creating' : 'Confirm'}</Text>
        </TouchableOpacity>
      </ScrollView>
    </View>
  );
}
