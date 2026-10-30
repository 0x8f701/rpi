// Shared token entry for one runner: the manual token field plus a "Scan QR"
// affordance that reads the host pairing QR through the same scanner the Add
// runner screen uses.
//
// The modal collects the token and nothing else: the owner supplies `busy` and
// performs the save, so the runner detail and the token_required workspace save
// through one path. The draft is reloaded from `initial` on every open, so a
// cancelled edit never leaks into the next open.

import React, { useEffect, useRef, useState } from 'react';
import { Modal, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { parseRunnerInviteToken } from '../protocol';
import { styles, type Palette } from '../ui';
import { QrScannerModal } from './QrScannerModal';

export function TokenEntryModal(props: {
  visible: boolean;
  palette: Palette;
  initial?: string;
  busy?: boolean;
  onSave: (token: string) => void;
  onCancel: () => void;
  overlayToast?: React.ReactNode;
}): React.JSX.Element {
  const { palette } = props;
  const [draft, setDraft] = useState('');
  const [scanOpen, setScanOpen] = useState(false);
  const initialRef = useRef(props.initial);
  initialRef.current = props.initial;

  useEffect(() => {
    if (!props.visible) return;
    setDraft(initialRef.current ?? '');
    setScanOpen(false);
  }, [props.visible]);

  const token = draft.trim();
  const busy = props.busy === true;

  return (
    <>
      <Modal visible={props.visible} transparent animationType="fade" onRequestClose={props.onCancel}>
        <View style={styles.modalScrim}>
          <View style={[styles.modalCard, { backgroundColor: palette.surface, borderColor: palette.border }]}>
            <Text style={[styles.rowTitle, { color: palette.fg }]}>Set token</Text>
            <TextInput
              style={[styles.input, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
              value={draft}
              onChangeText={setDraft}
              placeholder="listen token"
              placeholderTextColor={palette.muted}
              autoCapitalize="none"
              autoCorrect={false}
              secureTextEntry
              accessibilityLabel="Token"
            />
            <TouchableOpacity
              style={[styles.primaryButton, { borderWidth: 1, borderColor: palette.border, backgroundColor: palette.bg }]}
              onPress={() => setScanOpen(true)}
              accessibilityLabel="Scan QR">
              <Icon name="qr-code-scanner" size={20} color={palette.accent} />
              <Text style={[styles.primaryButtonText, { color: palette.fg }]}>Scan QR</Text>
            </TouchableOpacity>
            <TouchableOpacity
              style={[styles.primaryButton, { backgroundColor: palette.accent }, (token === '' || busy) && styles.faded]}
              disabled={token === '' || busy}
              onPress={() => props.onSave(token)}
              accessibilityLabel="Save">
              <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Save</Text>
            </TouchableOpacity>
            <TouchableOpacity onPress={props.onCancel} accessibilityLabel="Cancel">
              <Text style={[styles.emptyText, { color: palette.muted }]}>Cancel</Text>
            </TouchableOpacity>
          </View>
          {props.overlayToast}
        </View>
      </Modal>
      <QrScannerModal
        visible={scanOpen}
        palette={palette}
        onScanned={(value) => {
          setScanOpen(false);
          setDraft(parseRunnerInviteToken(value));
        }}
        onClose={() => setScanOpen(false)}
      />
    </>
  );
}
