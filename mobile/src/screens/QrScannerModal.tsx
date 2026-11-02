// Shared camera surface for reading a runner QR: the host pairing link
// (`{base}/chat#{token}`) or a bare listen token.
//
// The modal owns the full-screen black card and the close control; the camera
// body below owns the Android CAMERA permission and the single-shot read guard,
// and hands the raw scanned string to the caller. Nothing here submits.
//
// The caller closes the modal — from `onScanned` or from `onClose` — because
// only the caller knows whether the scan needs a follow-up (Add runner submits
// the whole link, token entry only fills a field).

import React, { useEffect, useRef, useState } from 'react';
import { Modal, PermissionsAndroid, Platform, Text, TouchableOpacity, View } from 'react-native';
import { Camera, CameraType } from 'react-native-camera-kit';
import Icon from 'react-native-vector-icons/MaterialIcons';
import { styles, type Palette } from '../ui';

export function QrScannerModal(props: {
  visible: boolean;
  palette: Palette;
  onScanned: (value: string) => void;
  onClose: () => void;
}): React.JSX.Element {
  const { palette } = props;
  return (
    <Modal visible={props.visible} animationType="slide" onRequestClose={props.onClose}>
      <View style={{ flex: 1, backgroundColor: '#000000' }}>
        {props.visible && <CameraSurface palette={palette} onScanned={props.onScanned} />}
        <TouchableOpacity
          style={{ position: 'absolute', top: 40, right: 20, padding: 8 }}
          onPress={props.onClose}
          accessibilityLabel="Close scanner">
          <Icon name="close" size={28} color={palette.fg} />
        </TouchableOpacity>
      </View>
    </Modal>
  );
}

/** One open's camera. Mounting IS the open: the instance is dropped when the
 *  modal closes (on iOS too, where the Modal shell outlives its children), so
 *  neither a previous open's grant nor its already-used read can reach the next
 *  open. The verdict starts unresolved and is only ever set by this instance's
 *  own request, so no render of a reopened modal can mount the camera off an
 *  earlier grant. */
function CameraSurface(props: {
  palette: Palette;
  onScanned: (value: string) => void;
}): React.JSX.Element {
  const { palette } = props;
  const [cameraAllowed, setCameraAllowed] = useState<boolean | null>(null);
  const scannedRef = useRef(false);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      if (Platform.OS !== 'android') {
        if (!cancelled) setCameraAllowed(true);
        return;
      }
      // A request that rejects proves nothing about the grant: treat it exactly
      // like a denial, so the camera is never mounted on a failed request and
      // no rejection escapes this effect.
      let granted: boolean;
      try {
        granted = await PermissionsAndroid.request(PermissionsAndroid.PERMISSIONS.CAMERA)
          === PermissionsAndroid.RESULTS.GRANTED;
      } catch {
        granted = false;
      }
      if (!cancelled) setCameraAllowed(granted);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (cameraAllowed !== true) {
    return (
      <Text style={[styles.emptyText, { color: palette.fg, padding: 24 }]}>
        Camera permission is required to scan.
      </Text>
    );
  }

  return (
    <Camera
      style={{ flex: 1 }}
      cameraType={CameraType.Back}
      scanBarcode
      onReadCode={(event) => {
        const value = event?.nativeEvent?.codeStringValue ?? '';
        if (scannedRef.current || !value) return;
        scannedRef.current = true;
        props.onScanned(value);
      }}
      showFrame={false}
      laserColor="transparent"
      frameColor="transparent"
    />
  );
}
