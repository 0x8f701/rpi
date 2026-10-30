import React from 'react';
import { StyleSheet, Text, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import type { RunnerConnectionState } from './runnerClient';

export const NERD_FONT_STACK = '"JetBrainsMono Nerd Font", "JetBrainsMono NF", "Symbols Nerd Font", monospace';

export interface Palette {
  bg: string;
  surface: string;
  border: string;
  fg: string;
  muted: string;
  accent: string;
  error: string;
  ok: string;
  mono: string;
}

export const DARK: Palette = {
  bg: '#101214',
  surface: '#1a1d20',
  border: '#2c3136',
  fg: '#e8eaed',
  muted: '#9aa0a6',
  accent: '#8ab4f8',
  error: '#f28b82',
  ok: '#81c995',
  mono: NERD_FONT_STACK,
};

export function statusDotColor(state: RunnerConnectionState, palette: Palette): string {
  switch (state) {
    case 'connected': return palette.ok;
    case 'token_required': return palette.error;
    case 'connecting': return palette.muted;
    case 'reconnecting': return palette.muted;
    default: return palette.muted;
  }
}

export function StatusDot(props: { state: RunnerConnectionState; palette: Palette }): React.JSX.Element {
  return (
    <View
      accessibilityLabel={props.state}
      style={[styles.dot, { backgroundColor: statusDotColor(props.state, props.palette) }]}
    />
  );
}

export function AppBar(props: {
  palette: Palette;
  title: string;
  onBack?: () => void;
  actions?: Array<{ icon: string; label: string; onPress: () => void; disabled?: boolean }>;
  leading?: React.ReactNode;
}): React.JSX.Element {
  const { palette } = props;
  return (
    <View style={[styles.appBar, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}>
      {props.onBack && (
        <TouchableOpacity style={styles.iconButton} onPress={props.onBack} accessibilityLabel="Back">
          <Icon name="arrow-back" size={24} color={palette.fg} />
        </TouchableOpacity>
      )}
      {props.leading}
      <View style={styles.appBarTitle}>
        <Text style={[styles.appBarTitleText, { color: palette.fg }]} numberOfLines={1}>{props.title}</Text>
      </View>
      {props.actions?.map((action) => (
        <TouchableOpacity
          key={action.label}
          style={[styles.iconButton, action.disabled && styles.faded]}
          disabled={action.disabled}
          onPress={action.onPress}
          accessibilityLabel={action.label}>
          <Icon name={action.icon} size={24} color={palette.fg} />
        </TouchableOpacity>
      ))}
    </View>
  );
}

export const styles = StyleSheet.create({
  screen: { flex: 1 },
  appBar: {
    flexDirection: 'row',
    alignItems: 'center',
    paddingHorizontal: 8,
    paddingVertical: 10,
    borderBottomWidth: 1,
    minHeight: 56,
  },
  appBarTitle: { flex: 1, minWidth: 0, marginLeft: 8 },
  appBarTitleText: { fontSize: 18, fontWeight: '600' },
  iconButton: { padding: 10 },
  faded: { opacity: 0.45 },
  list: { flex: 1 },
  listRow: {
    flexDirection: 'row',
    alignItems: 'center',
    paddingHorizontal: 16,
    paddingVertical: 12,
    borderBottomWidth: 1,
    minHeight: 64,
    gap: 12,
  },
  rowBody: { flex: 1, minWidth: 0 },
  rowTitle: { fontSize: 16 },
  rowSub: { fontSize: 13, marginTop: 2 },
  emptyBox: { alignItems: 'center', paddingVertical: 48, gap: 12 },
  emptyText: { fontSize: 14, textAlign: 'center' },
  inlineError: { paddingHorizontal: 16, paddingVertical: 8, fontSize: 13 },
  fab: {
    position: 'absolute',
    right: 20,
    bottom: 20,
    width: 56,
    height: 56,
    alignItems: 'center',
    justifyContent: 'center',
    elevation: 6,
  },
  tabBar: {
    flexDirection: 'row',
    borderTopWidth: 1,
    minHeight: 56,
  },
  tabItem: { flex: 1, alignItems: 'center', justifyContent: 'center', paddingVertical: 6, gap: 2 },
  tabLabel: { fontSize: 12 },
  card: {
    borderWidth: 1,
    padding: 12,
    marginHorizontal: 12,
    marginTop: 10,
  },
  cardHead: { flexDirection: 'row', alignItems: 'center', gap: 8, marginBottom: 8 },
  cardTitle: { fontSize: 15, fontWeight: '600', flex: 1 },
  cardMeta: { fontSize: 12 },
  cardChip: { fontSize: 11, borderWidth: 1, paddingHorizontal: 6, paddingVertical: 1 },
  cardPre: { fontSize: 11, lineHeight: 15 },
  table: { borderWidth: 1, marginVertical: 6 },
  tableRow: { flexDirection: 'row' },
  tableCell: { flex: 1, minWidth: 0, paddingHorizontal: 8, paddingVertical: 4, borderRightWidth: 1 },
  tableCellLast: { borderRightWidth: 0 },
  tableText: { fontSize: 13 },
  tableHeadText: { fontSize: 13, fontWeight: '700' },
  bottomPad: { height: 24 },
  form: { padding: 20, gap: 14 },
  formSection: { fontSize: 12, fontWeight: '700', letterSpacing: 0.5, textTransform: 'uppercase', marginTop: 8 },
  input: {
    borderWidth: 1,
    paddingHorizontal: 14,
    paddingVertical: 12,
    fontSize: 15,
  },
  primaryButton: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'center',
    gap: 8,
    paddingVertical: 14,
    marginTop: 8,
  },
  primaryButtonText: { fontSize: 15, fontWeight: '600' },
  snackbar: {
    position: 'absolute',
    left: 16,
    right: 16,
    bottom: 20,
    paddingHorizontal: 16,
    paddingVertical: 12,
    elevation: 6,
  },
  snackbarText: { fontSize: 13 },
  toastLayer: { flex: 1 },
  dot: { width: 10, height: 10 },
  modalScrim: {
    flex: 1,
    backgroundColor: 'rgba(0,0,0,0.55)',
    justifyContent: 'center',
    padding: 24,
  },
  modalCard: {
    borderWidth: 1,
    padding: 16,
    gap: 12,
  },
  bubbleUser: {
    alignSelf: 'flex-end',
    maxWidth: '86%',
    borderWidth: 1,
    padding: 10,
    marginHorizontal: 12,
    marginTop: 8,
  },
  bubbleRunner: {
    alignSelf: 'flex-start',
    maxWidth: '86%',
    borderWidth: 1,
    padding: 10,
    marginHorizontal: 12,
    marginTop: 8,
  },
  composerRow: {
    flexDirection: 'row',
    alignItems: 'center',
    borderTopWidth: 1,
    padding: 8,
    gap: 8,
  },
  composerInput: {
    flex: 1,
    borderWidth: 1,
    paddingHorizontal: 12,
    paddingVertical: 10,
    fontSize: 15,
    minHeight: 44,
  },
  attachButton: {
    width: 44,
    height: 44,
    borderWidth: 1,
  },
  attachmentBar: {
    borderTopWidth: 1,
    maxHeight: 78,
    flexGrow: 0,
  },
  attachmentBarContent: {
    alignItems: 'center',
    paddingHorizontal: 8,
    paddingVertical: 8,
    gap: 8,
  },
  attachmentChip: {
    flexDirection: 'row',
    alignItems: 'center',
    borderWidth: 1,
    height: 44,
    maxWidth: 240,
  },
  attachmentThumb: { width: 32, height: 32, marginLeft: 6 },
  attachmentChipIcon: { width: 32, alignItems: 'center', justifyContent: 'center', marginLeft: 6 },
  attachmentChipBody: { flexShrink: 1, minWidth: 0, marginLeft: 6 },
  attachmentChipName: { fontSize: 13, fontWeight: '600' },
  attachmentChipMeta: { fontSize: 11, marginTop: 2 },
  attachmentRemove: { width: 44, height: 44, alignItems: 'center', justifyContent: 'center' },
  softKeyRow: {
    flexDirection: 'row',
    alignItems: 'center',
    borderTopWidth: 1,
    paddingHorizontal: 4,
    paddingVertical: 4,
    gap: 4,
  },
  softKey: {
    flex: 1,
    minHeight: 40,
    alignItems: 'center',
    justifyContent: 'center',
    borderWidth: 1,
  },
  sideChatTabs: {
    flexDirection: 'row',
    flexWrap: 'wrap',
    alignItems: 'center',
    gap: 6,
    paddingHorizontal: 8,
    paddingVertical: 8,
    borderBottomWidth: 1,
  },
  sideChatTab: {
    flexDirection: 'row',
    alignItems: 'center',
    borderWidth: 1,
  },
  sideChatTabLabel: {
    paddingHorizontal: 8,
    paddingVertical: 8,
    fontSize: 13,
  },
  sideChatComposerInput: {
    flex: 1,
    borderWidth: 1,
    paddingHorizontal: 12,
    paddingVertical: 10,
    fontSize: 15,
    minHeight: 44,
    maxHeight: 88,
  },
});
