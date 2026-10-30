import React, { useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { AppBar, styles, type Palette } from '../ui';
import { QrScannerModal } from './QrScannerModal';

export function AddRunnerScreen(props: {
  palette: Palette;
  initialAddress?: string;
  onAdd: (raw: string) => Promise<boolean>;
  onClose: () => void;
}): React.JSX.Element {
  const { palette } = props;
  const [link, setLink] = useState(props.initialAddress ?? '');
  const [address, setAddress] = useState(props.initialAddress ?? '');
  const [token, setToken] = useState('');
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [scanOpen, setScanOpen] = useState(false);

  const submit = async (raw: string) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      await props.onAdd(raw);
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  const submitManual = async () => {
    const trimmedAddress = address.trim();
    const trimmedToken = token.trim();
    if (!trimmedAddress) return;
    const raw = trimmedToken ? `${trimmedAddress}#${encodeURIComponent(trimmedToken)}` : trimmedAddress;
    await submit(raw);
  };

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar palette={palette} title="Add runner" onBack={props.onClose} />
      <ScrollView contentContainerStyle={styles.form}>
        <TouchableOpacity
          style={[styles.card, { backgroundColor: palette.surface, borderColor: palette.border, alignItems: 'center', paddingVertical: 28 }]}
          onPress={() => setScanOpen(true)}
          accessibilityLabel="Scan QR">
          <Icon name="qr-code-scanner" size={44} color={palette.accent} />
          <Text style={[styles.rowTitle, { color: palette.fg }]}>Scan server QR</Text>
        </TouchableOpacity>
        <Text style={[styles.formSection, { color: palette.muted }]}>Invite link</Text>
        <TextInput
          style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
          value={link}
          onChangeText={setLink}
          placeholder="https://host:8765/chat#token"
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
          autoCorrect={false}
          inputMode="url"
        />
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent }, (link.trim() === '' || busy) && styles.faded]}
          disabled={link.trim() === '' || busy}
          onPress={() => void submit(link)}>
          <Icon name="link" size={20} color={palette.bg} />
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{busy ? 'Connecting' : 'Add from link'}</Text>
        </TouchableOpacity>
        <Text style={[styles.formSection, { color: palette.muted }]}>Manual address + token</Text>
        <TextInput
          style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
          value={address}
          onChangeText={setAddress}
          placeholder="https://host:8765/chat"
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
          autoCorrect={false}
        />
        <TextInput
          style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
          value={token}
          onChangeText={setToken}
          placeholder="listen token"
          placeholderTextColor={palette.muted}
          autoCapitalize="none"
          autoCorrect={false}
          secureTextEntry
        />
        <TouchableOpacity
          style={[styles.primaryButton, { backgroundColor: palette.accent }, (address.trim() === '' || busy) && styles.faded]}
          disabled={address.trim() === '' || busy}
          onPress={() => void submitManual()}>
          <Icon name="dns" size={20} color={palette.bg} />
          <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{busy ? 'Connecting' : 'Add runner'}</Text>
        </TouchableOpacity>
      </ScrollView>
      <QrScannerModal
        visible={scanOpen}
        palette={palette}
        onScanned={(value) => {
          setScanOpen(false);
          setLink(value);
          void submit(value);
        }}
        onClose={() => setScanOpen(false)}
      />
    </View>
  );
}
