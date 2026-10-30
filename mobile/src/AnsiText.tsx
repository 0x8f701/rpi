import React from 'react';
import { Text, type StyleProp, type TextStyle } from 'react-native';
import { ansiRgb, ansiToPlainText, parseAnsi } from './ansi';
import { redactSecrets } from './redact';

export function AnsiText(props: { text: string; style?: StyleProp<TextStyle>; numberOfLines?: number }): React.JSX.Element {
  const runs = parseAnsi(props.text);
  const plain = ansiToPlainText(props.text);
  const redacted = redactSecrets(plain);
  if (redacted !== plain) {
    return <Text style={props.style} numberOfLines={props.numberOfLines}>{redacted}</Text>;
  }
  return (
    <Text style={props.style} numberOfLines={props.numberOfLines}>
      {runs.map((run, index) => {
        const color = run.fgRgb ?? (run.fg !== undefined ? ansiRgb(run.fg) : undefined);
        const backgroundColor = run.bgRgb ?? (run.bg !== undefined ? ansiRgb(run.bg) : undefined);
        const runStyle: TextStyle = {
          color,
          backgroundColor,
          fontWeight: run.bold ? '700' : undefined,
          opacity: run.dim ? 0.65 : undefined,
          fontStyle: run.italic ? 'italic' : undefined,
          textDecorationLine: run.underline ? 'underline' : undefined,
        };
        return <Text key={index} style={runStyle}>{run.text}</Text>;
      })}
    </Text>
  );
}
