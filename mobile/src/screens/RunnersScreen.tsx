import React from 'react';
import { ScrollView, Text, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import type { DiscoveredRunner } from '../protocol';
import { runnerViewName, type RunnerView } from '../registry';
import { registry } from '../runtime';
import { TrustCertificate } from '../TrustCertificate';
import { AppBar, StatusDot, styles, type Palette } from '../ui';

export function RunnersScreen(props: {
  palette: Palette;
  runners: RunnerView[];
  lanCandidates: DiscoveredRunner[];
  onOpenRunner: (id: string) => void;
  onAdd: () => void;
  onAddLan: (runner: DiscoveredRunner) => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette } = props;
  return (
    <View style={styles.screen}>
      <AppBar palette={palette} title="Runners" />
      <ScrollView style={styles.list}>
        {props.runners.length === 0 && props.lanCandidates.length === 0 && (
          <View style={styles.emptyBox}>
            <Icon name="dns" size={40} color={palette.muted} />
            <Text style={[styles.emptyText, { color: palette.muted }]}>Add a runner to open its terminals.</Text>
          </View>
        )}
        {props.runners.map((runner) => (
          <TouchableOpacity
            key={runner.id}
            style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}
            onPress={() => props.onOpenRunner(runner.id)}>
            <StatusDot state={runner.state} palette={palette} />
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>{runnerViewName(runner)}</Text>
              {runner.origin.startsWith('https://') && runner.state !== 'connected' && (
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
            </View>
            <Icon name="chevron-right" size={24} color={palette.muted} />
          </TouchableOpacity>
        ))}
        {props.lanCandidates.map((candidate) => (
          <View
            key={candidate.origin}
            style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}>
            <StatusDot state="connecting" palette={palette} />
            <View style={styles.rowBody}>
              <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={1}>
                {candidate.codename || candidate.label}
              </Text>
              <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={1}>{candidate.origin}</Text>
            </View>
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent, marginTop: 0, paddingVertical: 8, paddingHorizontal: 12 }]}
              onPress={() => props.onAddLan(candidate)}
              accessibilityLabel="Add">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Add</Text>
            </TouchableOpacity>
          </View>
        ))}
        <View style={styles.bottomPad} />
      </ScrollView>
      <TouchableOpacity
        style={[styles.fab, { backgroundColor: palette.accent }]}
        onPress={props.onAdd}
        accessibilityLabel="Add runner">
        <Icon name="add" size={28} color={palette.bg} />
      </TouchableOpacity>
    </View>
  );
}
