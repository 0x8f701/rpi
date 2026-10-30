import React from 'react';
import { Text, TouchableOpacity, View } from 'react-native';
import { collapseSteeringPreview } from './steeringQueue';
import { styles, type Palette } from './ui';

const ACTION_TARGET = 44;

export function SteeringQueue(props: {
  palette: Palette;
  previews: string[];
  busy?: boolean;
  onSendNow: () => void;
  onCancelAt: (index: number) => void;
}): React.JSX.Element | null {
  if (props.previews.length === 0) return null;
  const { palette } = props;
  const locked = !!props.busy;
  return (
    <View testID="steering-queue" accessibilityLabel="Steering" style={{ flex: 1, minWidth: 0 }}>
      {props.previews.map((preview, index) => (
        <View
          key={`${index}:${preview}`}
          style={{
            flexDirection: 'row',
            alignItems: 'center',
            gap: 8,
            minHeight: ACTION_TARGET,
            paddingHorizontal: 8,
            paddingVertical: 4,
          }}>
          <Text style={[styles.cardMeta, { color: palette.muted, flexShrink: 0 }]}>Steering</Text>
          <Text
            numberOfLines={1}
            ellipsizeMode="tail"
            style={[styles.cardMeta, { color: palette.fg, flex: 1, minWidth: 0 }]}>
            {collapseSteeringPreview(preview)}
          </Text>
          <TouchableOpacity
            onPress={() => { if (locked) return; props.onSendNow(); }}
            disabled={locked}
            accessibilityLabel="Send all now"
            style={{
              minWidth: ACTION_TARGET,
              minHeight: ACTION_TARGET,
              alignItems: 'center',
              justifyContent: 'center',
            }}>
            <Text style={[styles.cardMeta, { color: palette.accent }]}>➤</Text>
          </TouchableOpacity>
          <TouchableOpacity
            onPress={() => { if (locked) return; props.onCancelAt(index); }}
            disabled={locked}
            accessibilityLabel="Remove steering message"
            style={{
              minWidth: ACTION_TARGET,
              minHeight: ACTION_TARGET,
              alignItems: 'center',
              justifyContent: 'center',
            }}>
            <Text style={[styles.cardMeta, { color: palette.muted }]}>×</Text>
          </TouchableOpacity>
        </View>
      ))}
    </View>
  );
}
