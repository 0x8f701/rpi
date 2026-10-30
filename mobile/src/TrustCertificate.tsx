import React, { useRef, useState } from 'react';
import { Modal, Pressable, Text, View } from 'react-native';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { formatTlsFingerprint, inspectRunnerTls, trustRunnerTls, type TlsInspect } from './tlsPin';
import { AppBar, styles, type Palette } from './ui';

export function TrustCertificate(props: {
  origin: string;
  palette: Palette;
  onError: (message: string) => void;
  onTrusted: () => void;
}): React.JSX.Element {
  const { palette } = props;
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const [preview, setPreview] = useState<TlsInspect | null>(null);

  const inspect = async () => {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      setPreview(await inspectRunnerTls(props.origin));
    } catch (err) {
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  const confirm = async () => {
    if (preview === null) return;
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(true);
    try {
      await trustRunnerTls(props.origin, preview.sha256);
      setPreview(null);
      props.onTrusted();
    } catch (err) {
      props.onError(err instanceof Error ? err.message : String(err));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  };

  return (
    <View>
      <Pressable onPress={() => { void inspect(); }} accessibilityLabel="Trust certificate" disabled={busy}>
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: 6 }}>
          <Icon name="security" size={18} color={palette.accent} />
          <Text style={[styles.rowSub, { color: palette.accent }]}>Trust certificate</Text>
        </View>
      </Pressable>
      <Modal visible={preview !== null} animationType="slide" onRequestClose={() => setPreview(null)}>
        <View style={[styles.screen, { backgroundColor: palette.bg }]}>
          <AppBar palette={palette} title="Trust certificate" onBack={() => setPreview(null)} />
          {preview && (
            <View style={styles.form}>
              <Text style={[styles.formSection, { color: palette.muted }]}>SHA-256</Text>
              <Text style={[styles.cardPre, { color: palette.fg, fontFamily: palette.mono }]}>{formatTlsFingerprint(preview.sha256)}</Text>
              <Text style={[styles.formSection, { color: palette.muted }]}>Subject</Text>
              <Text style={[styles.rowTitle, { color: palette.fg }]}>{preview.subject}</Text>
              <Text style={[styles.formSection, { color: palette.muted }]}>Issuer</Text>
              <Text style={[styles.rowTitle, { color: palette.fg }]}>{preview.issuer}</Text>
              <Pressable
                style={[styles.primaryButton, { backgroundColor: palette.accent }]}
                onPress={() => { void confirm(); }}
                accessibilityLabel="Confirm trust"
                disabled={busy}>
                <Icon name="verified-user" size={18} color={palette.bg} />
                <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Confirm</Text>
              </Pressable>
            </View>
          )}
        </View>
      </Modal>
    </View>
  );
}
